use super::*;
use crate::ops::{Cmp, Compare, Request, Response, Target};
use quepaxa::{Config, NodeId};

/// Executes the real node's persists and applies synchronously, leaving its peer frames
/// queued.
fn persist(node: &mut Node) {
    while !node.persists.is_empty() {
        let mut last = None;
        let mut then = Vec::new();
        for p in std::mem::take(&mut node.persists) {
            let disk = node.disk.as_mut().unwrap();
            if p.replace {
                disk.replace(&p.records).unwrap();
            } else {
                disk.append(&p.records).unwrap();
            }
            last = Some(p.id);
            then.extend(p.then);
        }
        node.pending = 0;
        node.written.advance(0, Some(Report { id: last.unwrap(), then, ok: true }));
        node.on_written();
    }
    node.apply_now();
}

fn request(node: &mut Node, req: Request) -> oneshot::Receiver<Reply> {
    let (reply, rx) = oneshot::channel();
    node.on_event(Ev::Client { cmd: Cmd::Write(req), reply });
    persist(node);
    rx
}

fn deliver(node: &mut Node, inbox: &mut tokio::sync::mpsc::Receiver<Vec<u8>>) -> bool {
    let mut delivered = false;
    while let Ok(bytes) = inbox.try_recv() {
        let frame = PeerFrame::decode(Bytes::from(bytes)).unwrap();
        node.on_event(Ev::Peer { from: 0, frame });
        persist(node);
        delivered = true;
    }
    delivered
}

#[test]
fn restarted_request_gets_its_own_result_when_incarnations_share_a_slot() {
    let root = std::env::temp_dir().join(format!("kv-tokio-incarnation-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (senders, mut inboxes): (Vec<_>, Vec<_>) = (0..3).map(|_| tokio::sync::mpsc::channel(128)).unzip();
    let open = |id: u32, seed| {
        let (disk, records) = Disk::open(&root.join(id.to_string())).unwrap();
        let mut cfg = Config::new([1; 16], NodeId(id), (0..3).map(NodeId).collect(), seed);
        cfg.max_in_flight_slots = 1;
        let replica = Replica::new(cfg, records, Timestamp(0)).unwrap();
        let peers = senders
            .iter()
            .enumerate()
            .map(|(id, tx)| (id as u32, PeerLink::new(tx.clone(), tokio::sync::mpsc::channel(1).0)))
            .collect();
        let (events, _) = tokio::sync::mpsc::channel(16);
        let mut node = Node::new(replica, disk, peers, u64::MAX, events);
        node.drain();
        persist(&mut node);
        node
    };
    let mut nodes: Vec<_> = (0..3).map(|id| open(id, [id as u8 + 1; 32])).collect();
    let txn = Request::Txn {
        compares: vec![Compare { key: b"lock".to_vec(), cmp: Cmp::Equal, target: Target::Version(0) }],
        success: vec![Request::Put { key: b"lock".to_vec(), value: b"held".to_vec(), prev_kv: false }],
        failure: Vec::new(),
    };
    // Keep slot 0 in flight so the leader batches both incarnations in slot 1.
    let _filler =
        request(&mut nodes[0], Request::Put { key: b"filler".to_vec(), value: Vec::new(), prev_kv: false });
    let _old = request(&mut nodes[1], txn.clone());
    deliver(&mut nodes[0], &mut inboxes[0]);
    nodes[1] = open(1, [9; 32]);
    let mut current = request(&mut nodes[1], txn);
    deliver(&mut nodes[0], &mut inboxes[0]);
    for _ in 0..100 {
        let mut progressed = false;
        for (node, inbox) in nodes.iter_mut().zip(&mut inboxes) {
            progressed |= deliver(node, inbox);
        }
        if !progressed {
            break;
        }
    }
    assert!(nodes.iter().all(|n| n.replica.commit_index() == 2), "both requests share slot 1");
    let Reply::Done(applied) = current.try_recv().expect("the new request committed") else {
        panic!("expected a transaction result");
    };
    assert_eq!(
        applied.response,
        Response::Txn { succeeded: false, responses: Vec::new() },
        "the old incarnation acquired the lock; the new request must see it held"
    );
    drop(nodes);
    std::fs::remove_dir_all(root).unwrap();
}

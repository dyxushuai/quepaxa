use super::*;
use crate::ops::{self, Request};

/// A single-member node whose persists are reported by hand (no writer thread).
fn node(name: &str) -> Node {
    let dir = std::env::temp_dir().join(format!("kv-tokio-node-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (disk, records) = Disk::open(&dir).unwrap();
    let cfg = quepaxa::Config::new([1; 16], quepaxa::NodeId(0), vec![quepaxa::NodeId(0)], [2; 32]);
    let replica = Replica::new(cfg, records, Timestamp(0)).unwrap();
    let (events, _) = tmpsc::channel(16);
    let mut n = Node::new(replica, disk, HashMap::new(), u64::MAX, events);
    n.drain();
    n
}

#[test]
fn every_report_reaches_the_replica_even_when_several_arrive_together() {
    let mut n = node("reports");
    let (reply, _) = oneshot::channel();
    n.on_client(
        n.now(),
        Cmd::Write(Request::Put { key: b"k".to_vec(), value: b"v".to_vec(), prev_kv: false }),
        reply,
    );
    n.drain();
    let id = n.persists.last().expect("the proposal persists").id;
    // Two reports taken in one turn: the first produces actions, the second is a failure.
    let report = |ok| Report { id, then: Vec::new(), ok };
    n.written.advance(0, Some(report(true)));
    n.written.advance(0, Some(report(false)));
    n.on_written();
    assert_eq!(
        n.replica.propose(n.now(), Bytes::from_static(b"x")),
        Err(quepaxa::Error::Fenced),
        "the storage failure fenced the replica"
    );
}

/// Acknowledges every persist issued so far, as the writer thread would.
fn settle(n: &mut Node) {
    while let Some(id) = n.persists.last().map(|p| p.id) {
        n.persists.clear();
        n.pending = 0;
        n.written.advance(0, Some(Report { id, then: Vec::new(), ok: true }));
        n.on_written();
    }
}

#[test]
fn each_write_gets_the_result_it_saw_in_log_order() {
    let mut n = node("results");
    let put = |n: &mut Node, v: &[u8]| {
        let (reply, rx) = oneshot::channel();
        let req = Request::Put { key: b"k".to_vec(), value: v.to_vec(), prev_kv: true };
        n.on_client(n.now(), Cmd::Write(req), reply);
        n.drain();
        rx
    };
    let mut first = put(&mut n, b"1");
    settle(&mut n);
    assert!(first.try_recv().is_err(), "answered before it was applied");
    n.apply_now();
    let mut second = put(&mut n, b"2");
    settle(&mut n);
    n.apply_now();
    let Ok(Reply::Done(a)) = first.try_recv() else { panic!("first write unanswered") };
    let Ok(Reply::Done(b)) = second.try_recv() else { panic!("second write unanswered") };
    assert_eq!(a.revision, 1);
    let ops::Response::Put { prev: Some(prev) } = b.response else { panic!("{b:?}") };
    assert_eq!((b.revision, &*prev.value), (2, &b"1"[..]));
}

/// A line-protocol request on `n`, taken in the current turn.
fn ask(n: &mut Node, cmd: Cmd) -> oneshot::Receiver<Reply> {
    let (reply, rx) = oneshot::channel();
    n.on_client(n.now(), cmd, reply);
    n.drain();
    rx
}

fn value(reply: Result<Reply, oneshot::error::TryRecvError>) -> Option<Vec<u8>> {
    let Ok(Reply::Done(ops::Applied { response: ops::Response::Range { kvs, .. }, .. })) = reply else {
        panic!("not a read result: {reply:?}")
    };
    kvs.first().map(|(_, e)| e.value.to_vec())
}

#[test]
fn a_linearizable_read_sees_every_write_decided_before_it() {
    let mut n = node("read-after-commit");
    let put = Request::Put { key: b"k".to_vec(), value: b"v".to_vec(), prev_kv: false };
    let mut write = ask(&mut n, Cmd::Write(put));
    // The write commits; the apply thread has not been handed it yet.
    settle(&mut n);
    let mut read = ask(&mut n, Cmd::Read { range: RangeReq::key(b"k"), linearizable: true });
    let mut stale = ask(&mut n, Cmd::Read { range: RangeReq::key(b"k"), linearizable: false });
    settle(&mut n);
    assert!(read.try_recv().is_err() && stale.try_recv().is_err(), "answered before the apply thread ran");
    n.apply_now();
    assert!(matches!(write.try_recv(), Ok(Reply::Done(_))));
    assert_eq!(value(read.try_recv()), Some(b"v".to_vec()), "the read missed a write decided before it");
    assert_eq!(
        value(stale.try_recv()),
        Some(b"v".to_vec()),
        "a stale read sees what was handed over before it"
    );
}

#[test]
fn a_snapshot_is_taken_at_the_slot_the_event_loop_names() {
    let mut n = node("snapshot-cut");
    let (events, mut saved) = tmpsc::channel(4);
    n.applier.as_mut().unwrap().0 =
        Applier::new(n.replica.id(), n.replica.incarnation(), n.snapshots.clone(), events);
    n.snapshot_every = 1;
    let put = |v: &[u8]| Cmd::Write(Request::Put { key: b"k".to_vec(), value: v.to_vec(), prev_kv: false });
    let _first = ask(&mut n, put(b"1"));
    settle(&mut n);
    n.maybe_snapshot();
    let through = n.applied - 1;
    // A later write handed over in the same batch is not in the snapshot.
    let _second = ask(&mut n, put(b"2"));
    settle(&mut n);
    n.apply_now();
    let Some(Ev::SnapshotSaved { through: t, bytes, saved: Ok(true), core }) = saved.blocking_recv() else {
        panic!("the snapshot was not saved")
    };
    assert_eq!(t, through);
    let snap = decode_snapshot(&bytes).unwrap();
    assert_eq!((snap.through, &*snap.kv.get(b"k").unwrap().value), (through, &b"1"[..]));
    assert_eq!(n.applier.as_ref().unwrap().0.kv().get(b"k").unwrap().value[..], b"2"[..]);
    // The log is compacted through the snapshot only once it is saved.
    n.on_event(Ev::SnapshotSaved { through: t, core, bytes, saved: Ok(true) });
    assert_eq!(n.own_snapshot.as_ref().map(|(t, _)| *t), Some(through));
}

#[test]
fn a_full_peer_queue_drops_frames_instead_of_blocking() {
    let mut n = node("full-peer");
    // Nothing drains these queues, like a peer that is unreachable.
    let (msgs, mut rx) = tokio::sync::mpsc::channel(1);
    let (bulk, _bulk) = tokio::sync::mpsc::channel(1);
    n.peers.insert(1, PeerLink::new(msgs, bulk));
    n.send(1, PeerFrame::Msg(Bytes::from_static(b"first")));
    n.send(1, PeerFrame::Msg(Bytes::from_static(b"second")));
    assert_eq!(rx.try_recv().unwrap(), PeerFrame::Msg(Bytes::from_static(b"first")).encode());
    assert!(rx.try_recv().is_err(), "the frame that did not fit was dropped");
}

#[test]
fn snapshots_take_the_bulk_queue_and_nothing_else_does() {
    let mut n = node("bulk-peer");
    let (msgs, mut msgs_rx) = tokio::sync::mpsc::channel(8);
    let (bulk, mut bulk_rx) = tokio::sync::mpsc::channel(1);
    n.peers.insert(1, PeerLink::new(msgs, bulk));
    let snapshot = || PeerFrame::Snapshot(Bytes::from_static(b"state"));
    n.send(1, snapshot());
    n.send(1, snapshot());
    // The full bulk queue does not hold messages back.
    n.send(1, PeerFrame::SnapshotRequest);
    n.send(1, PeerFrame::Msg(Bytes::from_static(b"m")));
    assert_eq!(bulk_rx.try_recv().unwrap(), snapshot().encode());
    assert!(bulk_rx.try_recv().is_err(), "the snapshot that did not fit was dropped");
    assert_eq!(msgs_rx.try_recv().unwrap(), PeerFrame::SnapshotRequest.encode());
    assert_eq!(msgs_rx.try_recv().unwrap(), PeerFrame::Msg(Bytes::from_static(b"m")).encode());
    assert!(msgs_rx.try_recv().is_err());
}

#[test]
fn waiting_for_the_disk_ends_once_the_writer_catches_up() {
    let written = Arc::new(Written::default());
    let writer = written.clone();
    let t = std::thread::spawn(move || {
        for bytes in [10, 20, 30] {
            std::thread::sleep(Duration::from_millis(5));
            writer.advance(bytes, None);
        }
    });
    written.wait_for(0);
    written.wait_for(25);
    assert!(written.lock().0 >= 25);
    t.join().unwrap();
    written.advance(u64::MAX, None);
    written.wait_for(u64::MAX - 1);
}

//! Bytes the leader sends per slot, by message kind, as issue #172 moves them.

use alloc::collections::{BTreeMap, VecDeque};

use super::*;

const CLUSTER: [u8; 16] = [3; 16];

fn wire() -> WireCtx {
    WireCtx { cluster_id: CLUSTER, config_epoch: 0, max_message_bytes: 16 << 20, max_value_bytes: 1 << 20 }
}

fn kind(bytes: &Bytes) -> &'static str {
    match Message::decode(bytes, &wire()).unwrap().body {
        Body::RecordRequest { .. } => "RecordRequest",
        Body::RecordReply { .. } => "RecordReply",
        Body::Decided { .. } => "Decided",
        Body::CatchUp { .. } => "CatchUp",
        Body::Entries { .. } => "Entries",
        Body::Forward { .. } => "Forward",
        Body::ForwardAck => "ForwardAck",
        Body::Compacted { .. } => "Compacted",
        Body::ReadQuery { .. } => "ReadQuery",
        Body::ReadReply { .. } => "ReadReply",
        Body::RttProbe { .. } => "RttProbe",
        Body::RttReply { .. } => "RttReply",
        Body::Data { .. } => "Data",
        Body::Fetch { .. } => "Fetch",
    }
}

/// `n` replicas on a network that delivers everything in order and acknowledges every
/// persist at once; time stands still, so no timer fires.
struct Net {
    rs: Vec<Replica>,
    /// Bytes and messages sent by replica 0 (the leader), by kind.
    leader_egress: BTreeMap<&'static str, (usize, usize)>,
}

impl Net {
    fn new(n: u32) -> Self {
        let rs = (0..n)
            .map(|id| {
                let cfg = Config::new(CLUSTER, NodeId(id), (0..n).map(NodeId).collect(), [id as u8 + 1; 32]);
                Replica::new(cfg, [], Timestamp(0)).unwrap()
            })
            .collect();
        let mut net = Self { rs, leader_egress: BTreeMap::new() };
        net.run();
        net.leader_egress.clear();
        net
    }

    fn run(&mut self) {
        let mut queue = VecDeque::new();
        let mut out = Vec::new();
        loop {
            for i in 0..self.rs.len() {
                self.rs[i].poll_actions(&mut out);
                for a in out.drain(..) {
                    let sends = match a {
                        Action::Send(o) => alloc::vec![o],
                        Action::Persist { id, then, .. } => {
                            queue.push_back((i, Input::Persisted(id)));
                            then
                        }
                        _ => continue,
                    };
                    for o in sends {
                        if i == 0 {
                            let e = self.leader_egress.entry(kind(&o.bytes)).or_default();
                            e.0 += o.bytes.len();
                            e.1 += 1;
                        }
                        queue.push_back((o.to.0 as usize, Input::Message(o.bytes)));
                    }
                }
            }
            let Some((to, input)) = queue.pop_front() else { return };
            self.rs[to].handle(Timestamp(0), input).unwrap();
        }
    }
}

/// Leader egress per slot carrying a slot value of `v` bytes: each of the n−1 others gets one
/// `RecordRequest` (44 envelope, 8 slot, 4 step, 28 head, 5 value tag and length, then v) and
/// one `Decided` naming the value by key (44, 8, 28), so (n−1)(169 + v). Before `Decided`
/// dropped the value (#172 Phase 1) it also carried a tag, a length and v: (n−1)(174 + 2v).
/// The 44-byte `ForwardAck` to the origin is rate-limited (`ack_every`), so it is not per
/// slot: with time standing still only the first slot sends one, and the last slot is the one
/// measured.
fn expected(n: usize, v: usize) -> BTreeMap<&'static str, (usize, usize)> {
    BTreeMap::from([("Decided", ((n - 1) * 80, n - 1)), ("RecordRequest", ((n - 1) * (89 + v), n - 1))])
}

#[test]
fn leader_egress_bytes_per_slot() {
    const SLOTS: usize = 4;
    for n in [3, 5] {
        for payload in [17, 64 << 10] {
            let mut net = Net::new(n as u32);
            let mut per_slot = BTreeMap::new();
            for s in 0..SLOTS {
                let committed = net.rs[0].emitted_index();
                // One forwarded chunk per slot, from a follower origin.
                net.rs[1].propose(Timestamp(0), Bytes::from(alloc::vec![s as u8; payload])).unwrap();
                net.run();
                assert_eq!(net.rs[0].emitted_index(), committed + 1, "n={n} payload={payload}: one slot");
                per_slot = core::mem::take(&mut net.leader_egress);
            }
            // The slot value is one batch of one entry: 4-byte count + 25-byte entry header.
            let v = 29 + payload;
            assert_eq!(per_slot, expected(n, v), "n={n} payload={payload}");
            let total: usize = per_slot.values().map(|(b, _)| b).sum();
            assert_eq!(total, (n - 1) * (169 + v), "n={n} payload={payload}");
        }
    }
}

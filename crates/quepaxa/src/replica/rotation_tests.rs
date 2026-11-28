//! Regression tests for issue #240: healthy leader rotation on brief loopback hiccups,
//! lagging-leader egress amplification, and handover routing during miss-based rotation.

use alloc::collections::BinaryHeap;
use core::cmp::Reverse;

use super::tests::{proposal, r_wire, replica_as};
use super::*;

const US: u64 = 1_000;
const MS: u64 = 1_000_000;
const CLUSTER: [u8; 16] = [9; 16];

fn wire() -> WireCtx {
    WireCtx { cluster_id: CLUSTER, config_epoch: 0, max_message_bytes: 16 << 20, max_value_bytes: 1 << 20 }
}

/// Deterministic microsecond-resolution 3-node loopback harness modeling kv-tokio on tmpfs.
struct Loopback {
    rs: [Replica; 3],
    queue: BinaryHeap<Reverse<(u64, u64, Ev)>>,
    seq: u64,
    now: u64,
    /// Events targeting node `i` are held until `paused_until[i]`.
    paused_until: [u64; 3],
    egress_bytes: [u64; 3],
    record_reqs: [u64; 3],
    rtt_half_us: u64,
    persist_us: u64,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Ev {
    Msg { to: usize, bytes: Vec<u8> },
    Persist { node: usize, id: u64, then: Vec<(usize, Vec<u8>)> },
    Timer { node: usize, at: u64 },
    Client { node: usize, payload: Vec<u8> },
}

impl Loopback {
    fn new(rtt_half_us: u64, persist_us: u64) -> Self {
        let members = alloc::vec![NodeId(0), NodeId(1), NodeId(2)];
        let mk = |id: u32| {
            let cfg = Config::new(CLUSTER, NodeId(id), members.clone(), [id as u8 + 1; 32]);
            Replica::new(cfg, [], Timestamp(0)).unwrap()
        };
        let mut lb = Self {
            rs: [mk(0), mk(1), mk(2)],
            queue: BinaryHeap::new(),
            seq: 0,
            now: 0,
            paused_until: [0; 3],
            egress_bytes: [0; 3],
            record_reqs: [0; 3],
            rtt_half_us,
            persist_us,
        };
        for i in 0..3 {
            lb.drain(i);
        }
        lb
    }

    fn push(&mut self, at: u64, ev: Ev) {
        self.seq += 1;
        self.queue.push(Reverse((at, self.seq, ev)));
    }

    fn send(&mut self, from: usize, o: Outbound) {
        self.egress_bytes[from] += o.bytes.len() as u64;
        if let Ok(Message { body: Body::RecordRequest { .. }, .. }) = Message::decode(&o.bytes, &wire()) {
            self.record_reqs[from] += 1;
        }
        let deliver = self.now + self.rtt_half_us * US;
        self.push(deliver, Ev::Msg { to: o.to.0 as usize, bytes: o.bytes.to_vec() });
    }

    fn drain(&mut self, i: usize) {
        let mut out = Vec::new();
        self.rs[i].poll_actions(&mut out);
        for a in out {
            match a {
                Action::Send(o) => self.send(i, o),
                Action::Persist { id, then, .. } => {
                    let gated = then.into_iter().map(|o| (o.to.0 as usize, o.bytes.to_vec())).collect();
                    let done = self.now + self.persist_us * US;
                    self.push(done, Ev::Persist { node: i, id: id.0, then: gated });
                }
                _ => {}
            }
        }
        if let Some(Timestamp(at)) = self.rs[i].next_deadline() {
            let at = at.max(self.now);
            self.push(at, Ev::Timer { node: i, at });
        }
    }

    fn run_until(&mut self, until: u64) {
        while let Some(Reverse((at, _, _))) = self.queue.peek() {
            if *at > until {
                break;
            }
            let Reverse((at, _, ev)) = self.queue.pop().unwrap();
            let target = match &ev {
                Ev::Msg { to, .. } => *to,
                Ev::Persist { node, .. } => *node,
                Ev::Timer { node, .. } => *node,
                Ev::Client { node, .. } => *node,
            };
            if at < self.paused_until[target] {
                self.push(self.paused_until[target], ev);
                continue;
            }
            self.now = at;
            match ev {
                Ev::Msg { to, bytes } => {
                    self.rs[to].handle(Timestamp(at), Input::Message(Bytes::from(bytes))).unwrap();
                    self.drain(to);
                }
                Ev::Persist { node, id, then } => {
                    for (to, bytes) in then {
                        self.send(node, Outbound { to: NodeId(to as u32), bytes: Bytes::from(bytes) });
                    }
                    self.rs[node].handle(Timestamp(at), Input::Persisted(PersistId(id))).unwrap();
                    self.drain(node);
                }
                Ev::Timer { node, at: scheduled } => {
                    if self.rs[node].next_deadline().is_some_and(|d| d.0 <= scheduled) {
                        self.rs[node].handle_timeout(Timestamp(at)).unwrap();
                        self.drain(node);
                    }
                }
                Ev::Client { node, payload } => {
                    let _ = self.rs[node].propose(Timestamp(at), Bytes::from(payload));
                    self.drain(node);
                }
            }
        }
        self.now = until;
    }

    fn feed_writes(&mut self, from_us: u64, to_us: u64, interval_us: u64, payload_bytes: usize) {
        let mut t = from_us;
        let mut idx = 0usize;
        while t < to_us {
            self.push(t * US, Ev::Client { node: idx % 3, payload: alloc::vec![0xAB; payload_bytes] });
            idx += 1;
            t += interval_us;
        }
        self.run_until(to_us * US);
    }
}

/// Persists every pending write immediately and returns all `Outbound`s emitted.
fn drain_persisting(r: &mut Replica, at: Timestamp) -> Vec<Outbound> {
    let mut sent = Vec::new();
    let mut out = Vec::new();
    loop {
        r.poll_actions(&mut out);
        if out.is_empty() {
            return sent;
        }
        let mut persisted = Vec::new();
        for a in out.drain(..) {
            match a {
                Action::Send(o) => sent.push(o),
                Action::Persist { id, then, .. } => {
                    sent.extend(then);
                    persisted.push(id);
                }
                _ => {}
            }
        }
        for id in persisted {
            r.handle(at, Input::Persisted(id)).unwrap();
        }
    }
}

#[test]
fn lagging_leader_skips_slots_already_decided_by_peers() {
    let mut r = replica_as(0);
    r.propose(Timestamp(MS), Bytes::from(alloc::vec![0xCC; 4096])).unwrap();
    drain_persisting(&mut r, Timestamp(MS));
    // Queue 16 * 4 KiB local writes while peers are far ahead (commit_index = 100).
    for _ in 0..16 {
        r.propose(Timestamp(2 * MS), Bytes::from(alloc::vec![0xCC; 4096])).unwrap();
        drain_persisting(&mut r, Timestamp(2 * MS));
    }
    let make_msg = |body: Body| {
        let env = Envelope {
            cluster_id: r_wire().cluster_id,
            config_epoch: 0,
            from: NodeId(1),
            to: NodeId(0),
            commit_index: 100,
        };
        Input::Message(Message { env, body }.encode())
    };
    let mut req_msgs = 0usize;
    let mut forwarded = 0usize;
    for slot in 0..72u64 {
        let p = proposal(1, 1, slot);
        let body = Body::Entries { first: Slot(slot), proposals: alloc::vec![p], content: Vec::new() };
        r.handle(Timestamp(3 * MS + slot), make_msg(body)).unwrap();
        for o in drain_persisting(&mut r, Timestamp(3 * MS + slot)) {
            match Message::decode(&o.bytes, &r_wire()).unwrap().body {
                Body::RecordRequest { .. } => req_msgs += 1,
                Body::Forward { .. } => forwarded += 1,
                _ => {}
            }
        }
    }
    assert_eq!(req_msgs, 0, "must not propose batches into slots peers already decided");
    assert!(forwarded > 0, "queued entries are handed on to the incoming leader once rotation is seen");
    r.check_entries().unwrap();
}

#[test]
fn brief_loopback_hiccup_does_not_rotate_or_hedge_a_healthy_leader() {
    // 100 us one-way (~200 us RTT), 100 us persist -> ~300 us commit latency (tmpfs loopback).
    let mut lb = Loopback::new(100, 100);
    lb.feed_writes(0, 30_000, 250, 4096);
    let before_hedged = [lb.rs[0].metrics().hedged, lb.rs[1].metrics().hedged, lb.rs[2].metrics().hedged];
    // A 15 ms scheduling pause on the leader must not trigger hedging, step-aside, or rotation.
    lb.paused_until[0] = 45 * MS;
    lb.feed_writes(30_000, 90_000, 250, 4096);
    lb.run_until(110_000 * US);
    let leaders: Vec<u32> = lb.rs[0].members().iter().map(|m| m.id.0).collect();
    assert_eq!(leaders, [0, 1, 2], "a 15 ms hiccup must not rotate out a healthy leader");
    for i in 0..3 {
        assert_eq!(lb.rs[i].metrics().hedged, before_hedged[i], "n{i} hedged on a 15 ms hiccup");
        assert_eq!(lb.rs[i].metrics().stepped_aside, 0, "n{i} stepped aside on a 15 ms hiccup");
        lb.rs[i].check_entries().unwrap();
    }
}

#[test]
fn pending_miss_rotation_routes_to_next_leader_without_egress_blast() {
    let mut lb = Loopback::new(100, 100);
    lb.feed_writes(0, 30_000, 250, 4096);
    let before_bytes = lb.egress_bytes[0];
    let before_reqs = lb.record_reqs[0];
    let before_hedged = [lb.rs[1].metrics().hedged, lb.rs[2].metrics().hedged];
    // A 100 ms leader pause legitimately triggers rotation; once 8 missed slots are seen,
    // replicas route directly to node 1 rather than blasting through all 64 LAG slots.
    lb.paused_until[0] = 130 * MS;
    lb.feed_writes(30_000, 180_000, 250, 4096);
    lb.run_until(200_000 * US);
    let leaders: Vec<u32> = lb.rs[0].members().iter().map(|m| m.id.0).collect();
    assert_eq!(leaders, [1, 2, 0], "a 100 ms pause rotates the leader to node 1");
    let d_reqs = lb.record_reqs[0] - before_reqs;
    let d_bytes = lb.egress_bytes[0] - before_bytes;
    assert!(d_reqs < 60, "node 0 sent {d_reqs} RecordRequests instead of stopping at rotation");
    assert!(d_bytes < 5_000_000, "node 0 blasted {d_bytes} bytes during catch-up");
    for (idx, prev) in [(1, before_hedged[0]), (2, before_hedged[1])] {
        let dh = lb.rs[idx].metrics().hedged - prev;
        assert!(dh < 80, "n{idx} hedged {dh} times during handover");
    }
    for r in &lb.rs {
        r.check_entries().unwrap();
    }
}

#[test]
fn routing_order_switches_early_only_for_miss_based_rotation() {
    let mut s = Schedule::with_order(alloc::vec![NodeId(0), NodeId(1), NodeId(2)]);
    // Planned leader-order change at slot 2 takes effect at 2 + LAG: keep routing to 0 until then.
    assert!(s.reconfigure(Slot(2), 0, alloc::vec![NodeId(1), NodeId(2), NodeId(0)]).is_some());
    assert_eq!(s.routing_order(Slot(3)), &[NodeId(0), NodeId(1), NodeId(2)]);
    assert_eq!(s.routing_order(Slot(2 + crate::schedule::LAG)), &[NodeId(1), NodeId(2), NodeId(0)]);

    // Miss-based rotation after 8 missed slots (0..=7): visible only at slot >= 8, so a
    // lagging peer at slot <= 7 still routes to node 0 while committed >= 8 routes to node 1.
    let mut rot = Schedule::with_order(alloc::vec![NodeId(0), NodeId(1), NodeId(2)]);
    for slot in 0..8 {
        rot.observe(Slot(slot), NodeId(1), false);
    }
    assert_eq!(rot.leader(Slot(8)), NodeId(0), "fast-path leader stays 0 until 7 + LAG");
    assert_eq!(rot.routing_order(Slot(5)), &[NodeId(0), NodeId(1), NodeId(2)], "lagging peer at 5");
    assert_eq!(rot.routing_order(Slot(7)), &[NodeId(0), NodeId(1), NodeId(2)], "lagging peer at 7");
    assert_eq!(
        rot.routing_order(Slot(8)),
        &[NodeId(1), NodeId(2), NodeId(0)],
        "proposals route to the incoming leader during the LAG window"
    );
    assert_eq!(rot.routing_order(Slot(7 + crate::schedule::LAG)), &[NodeId(1), NodeId(2), NodeId(0)]);
}

#[test]
fn outgoing_leader_hands_on_forwards_from_lagging_follower_during_pending_rotation() {
    let mut r = replica_as(0);
    drain_persisting(&mut r, Timestamp(MS));
    // Observe 8 missed slots (0..=7) on leader 0 so miss-based rotation to [1, 2, 0] is pending.
    for slot in 0..8u64 {
        let p = proposal(1, 1, slot);
        let env = Envelope {
            cluster_id: r_wire().cluster_id,
            config_epoch: 0,
            from: NodeId(1),
            to: NodeId(0),
            commit_index: slot + 1,
        };
        let body = Body::Entries { first: Slot(slot), proposals: alloc::vec![p], content: Vec::new() };
        r.handle(Timestamp(2 * MS + slot), Input::Message(Message { env, body }.encode())).unwrap();
        drain_persisting(&mut r, Timestamp(2 * MS + slot));
    }
    assert_eq!(r.committed, 8);
    assert_eq!(r.target(), NodeId(1));
    // A lagging follower (commit_index = 5) still forwards to node 0: node 0 must hand the
    // entry on to node 1 instead of proposing it itself as a deputy.
    let item = Item {
        entry: Entry {
            origin: NodeId(2),
            incarnation: 1,
            id: ProposalId(42),
            payload: Bytes::from_static(b"hi"),
        },
        kind: Kind::App,
    };
    let env = Envelope {
        cluster_id: r_wire().cluster_id,
        config_epoch: 0,
        from: NodeId(2),
        to: NodeId(0),
        commit_index: 5,
    };
    let body = Body::Forward { seq: 0, batch: batch::encode(&[item]) };
    r.handle(Timestamp(3 * MS), Input::Message(Message { env, body }.encode())).unwrap();
    let sent = drain_persisting(&mut r, Timestamp(3 * MS));
    let forwarded_to_1 = sent.iter().any(|o| {
        o.to == NodeId(1)
            && matches!(Message::decode(&o.bytes, &r_wire()), Ok(Message { body: Body::Forward { .. }, .. }))
    });
    assert!(forwarded_to_1, "outgoing leader must hand lagging follower's forward on to node 1");
    assert!(r.proposers.is_empty(), "outgoing leader must not propose the entry itself");
    r.check_entries().unwrap();
}

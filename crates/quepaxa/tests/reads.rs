//! Linearizable reads through the public API: a quorum round, no log slot, no write.

mod common;

use bytes::Bytes;
use common::{config, drain};
use quepaxa::{Action, DropReason, Error, Input, NodeId, Replica, Slot, Timestamp};

/// Replicas on a network that delivers everything except to the ones that are down, and
/// acknowledges every persist; time moves only to fire timers.
struct Net {
    rs: Vec<Replica>,
    now: Timestamp,
    /// Every action other than sends and persists, by replica, in order.
    seen: Vec<(usize, Action)>,
    persists: usize,
}

impl Net {
    /// Replicas `0..n` of a cluster whose configured members are 0, 1 and 2.
    fn new(n: u32) -> Self {
        let rs = (0..n)
            .map(|id| {
                let mut cfg = config(&[0, 1, 2]);
                cfg.id = NodeId(id);
                Replica::new(cfg, [], Timestamp(0)).unwrap()
            })
            .collect();
        let mut net = Self { rs, now: Timestamp(0), seen: Vec::new(), persists: 0 };
        net.run(&[]);
        net
    }

    /// Delivers until quiet, firing timers as long as something is due within a second.
    fn run(&mut self, down: &[usize]) {
        for _ in 0..1000 {
            self.deliver(down);
            let next = self
                .rs
                .iter()
                .enumerate()
                .filter(|(i, _)| !down.contains(i))
                .filter_map(|(_, r)| r.next_deadline())
                .min();
            match next {
                Some(t) if t.0 <= self.now.0 + 1_000_000_000 => {
                    self.now = Timestamp(t.0.max(self.now.0));
                    for (i, r) in self.rs.iter_mut().enumerate() {
                        if !down.contains(&i) && r.next_deadline().is_some_and(|d| d <= self.now) {
                            r.handle_timeout(self.now).unwrap();
                        }
                    }
                }
                _ => return,
            }
        }
    }

    fn deliver(&mut self, down: &[usize]) {
        let mut queue = std::collections::VecDeque::new();
        loop {
            for (i, r) in self.rs.iter_mut().enumerate() {
                for a in drain(r) {
                    match a {
                        Action::Send(o) => queue.push_back((o.to.0 as usize, Input::Message(o.bytes))),
                        Action::Persist { id, then, .. } => {
                            self.persists += 1;
                            queue
                                .extend(then.into_iter().map(|o| (o.to.0 as usize, Input::Message(o.bytes))));
                            queue.push_back((i, Input::Persisted(id)));
                        }
                        other => self.seen.push((i, other)),
                    }
                }
            }
            let Some((to, input)) = queue.pop_front() else { return };
            if down.contains(&to) && !matches!(input, Input::Persisted(_)) {
                continue;
            }
            self.rs[to].handle(self.now, input).unwrap();
        }
    }

    fn write(&mut self, at: usize, down: &[usize]) -> Slot {
        let id = self.rs[at].propose(self.now, Bytes::from_static(b"w")).unwrap();
        self.run(down);
        self.seen
            .iter()
            .find_map(|(i, a)| match a {
                Action::Committed { id: c, slot } if *i == at && *c == id => Some(*slot),
                _ => None,
            })
            .expect("the write committed")
    }

    /// Where read `id` at replica `at` was reported ready, and its position among the
    /// actions seen.
    fn ready(&self, at: usize, id: quepaxa::ProposalId) -> Option<(usize, Slot)> {
        self.seen.iter().enumerate().find_map(|(n, (i, a))| match a {
            Action::ReadReady { id: r, slot } if *i == at && *r == id => Some((n, *slot)),
            _ => None,
        })
    }
}

#[test]
fn a_read_elsewhere_covers_a_completed_write_without_writing_anything() {
    let mut net = Net::new(3);
    let w = net.write(0, &[]);
    let (decided, persists) = (net.rs[0].commit_index(), net.persists);
    let read = net.rs[2].read_barrier(net.now).unwrap();
    net.run(&[]);
    let (at, slot) = net.ready(2, read).expect("the read is ready");
    assert!(slot >= w);
    // The commit of the write's slot reached replica 2 before the read did.
    let commit =
        net.seen.iter().position(|(i, a)| *i == 2 && matches!(a, Action::Commit { slot, .. } if *slot == w));
    assert!(commit.is_some_and(|c| c < at));
    assert_eq!((net.rs[0].commit_index(), net.persists), (decided, persists), "no slot, no write");
}

#[test]
fn a_lagging_replica_catches_up_before_its_read_is_ready() {
    let mut net = Net::new(3);
    let mut last = Slot(0);
    for _ in 0..5 {
        last = net.write(0, &[2]);
    }
    assert!(net.rs[2].commit_index() <= last.0);
    let read = net.rs[2].read_barrier(net.now).unwrap();
    net.run(&[]);
    let (_, slot) = net.ready(2, read).expect("the read is ready");
    assert!(slot >= last && net.rs[2].commit_index() > last.0);
}

#[test]
fn the_first_read_of_a_fresh_cluster_waits_for_slot_zero() {
    let mut net = Net::new(3);
    let read = net.rs[1].read_barrier(net.now).unwrap();
    net.run(&[]);
    assert_eq!(net.ready(1, read).map(|(_, s)| s), Some(Slot(0)));
    assert!(net.rs[1].commit_index() >= 1, "slot 0 was decided, empty");
}

#[test]
fn reads_need_a_majority_and_wait_while_it_is_missing() {
    let mut net = Net::new(3);
    net.write(0, &[]);
    let read = net.rs[0].read_barrier(net.now).unwrap();
    net.run(&[1, 2]);
    assert_eq!(net.ready(0, read), None, "the replica's own answer is not a majority");
    net.run(&[2]);
    assert!(net.ready(0, read).is_some());
}

#[test]
fn reads_requested_together_share_one_round_and_count_against_the_limit() {
    let mut net = Net::new(3);
    let mut sent = 0;
    let mut ids = Vec::new();
    for _ in 0..10 {
        ids.push(net.rs[2].read_barrier(net.now).unwrap());
        sent += drain(&mut net.rs[2]).iter().filter(|a| matches!(a, Action::Send(_))).count();
    }
    assert_eq!(sent, 2, "one query to each other member");
    net.run(&[]);
    assert!(ids.iter().all(|&id| net.ready(2, id).is_some()));

    let mut cfg = config(&[0]);
    cfg.max_outstanding_proposals = 3;
    let mut r = Replica::new(cfg, [], Timestamp(0)).unwrap();
    common::settle(&mut r, &mut Vec::new());
    r.propose(Timestamp(0), Bytes::new()).unwrap();
    drain(&mut r);
    for _ in 0..2 {
        r.read_barrier(Timestamp(0)).unwrap();
        drain(&mut r);
    }
    assert_eq!(r.read_barrier(Timestamp(0)), Err(Error::Busy));
}

#[test]
fn fencing_drops_pending_reads() {
    let mut r = Replica::new(config(&[0, 1, 2]), [], Timestamp(0)).unwrap();
    common::settle(&mut r, &mut Vec::new());
    let write = r.propose(Timestamp(0), Bytes::new()).unwrap();
    let mut actions = drain(&mut r);
    let read = r.read_barrier(Timestamp(0)).unwrap();
    actions.extend(drain(&mut r));
    let persist = actions
        .into_iter()
        .find_map(|a| match a {
            Action::Persist { id, .. } => Some(id),
            _ => None,
        })
        .expect("the write is persisted");
    r.handle(Timestamp(0), Input::StorageFailed(persist)).unwrap();
    let dropped: Vec<_> = drain(&mut r)
        .into_iter()
        .filter_map(|a| match a {
            Action::Dropped { id, reason: DropReason::Fenced } => Some(id),
            _ => None,
        })
        .collect();
    assert!(dropped.contains(&write) && dropped.contains(&read), "{dropped:?}");
    assert_eq!(r.read_barrier(Timestamp(0)), Err(Error::Fenced));
}

#[test]
fn a_read_spanning_a_pending_change_needs_a_majority_of_both_member_sets() {
    // Replica 3 joins; the change is decided but not yet in force.
    let mut net = Net::new(4);
    net.rs[0].propose_reconfiguration(net.now, vec![NodeId(0), NodeId(1), NodeId(3)], vec![]).unwrap();
    // No timers: nobody fills the slots up to the new epoch yet.
    net.deliver(&[]);
    assert!(net.seen.iter().any(|(i, a)| *i == 0 && matches!(a, Action::Committed { .. })));
    let from = net
        .seen
        .iter()
        .find_map(|(i, a)| match a {
            Action::Reconfigured { from, .. } if *i == 0 => Some(*from),
            _ => None,
        })
        .expect("the change is decided");
    assert!(net.rs[0].commit_index() < from.0, "and not yet in force");
    // Of the old members {0, 1, 2} only 0 answers; the new {0, 1, 3} have a majority.
    let read = net.rs[0].read_barrier(net.now).unwrap();
    net.run(&[1, 2]);
    assert_eq!(net.ready(0, read), None);
    net.run(&[1]);
    assert!(net.ready(0, read).is_some());
}

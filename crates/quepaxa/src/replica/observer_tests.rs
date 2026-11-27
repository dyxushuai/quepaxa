//! Non-voting observer replica tests (#157): continuous log following, linearizable reads,
//! non-retirement across reconfigurations, compaction/snapshots, peer failover, restart, and
//! promotion to voting member.

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use bytes::Bytes;

use super::tests::r_wire;
use super::*;
use crate::api::{Action, Config, Input};
use crate::codec::{Body, Message};
use crate::types::{NodeId, Slot, Timestamp};

const MS: u64 = 1_000_000;

struct MiniCluster {
    replicas: Vec<Option<Replica>>,
    disks: Vec<Vec<Bytes>>,
    retired: Vec<Option<u64>>,
    read_ready: Vec<Vec<(ProposalId, Slot)>>,
    snapshot_needed: Vec<Vec<(NodeId, Slot)>>,
    now: Timestamp,
}

impl MiniCluster {
    fn new(voters: &[u32], observers: &[u32], follow_ms: u64) -> Self {
        let total = voters.iter().chain(observers).copied().max().unwrap_or(0) as usize + 1;
        let members: Vec<NodeId> = voters.iter().copied().map(NodeId).collect();
        let mut replicas: Vec<Option<Replica>> = (0..total).map(|_| None).collect();
        for &id in voters.iter().chain(observers) {
            let mut cfg = Config::new([3; 16], NodeId(id), members.clone(), [id as u8 + 1; 32]);
            if observers.contains(&id) {
                cfg.follow_interval = follow_ms * MS;
            }
            replicas[id as usize] = Some(Replica::new(cfg, [], Timestamp(0)).unwrap());
        }
        let mut c = Self {
            replicas,
            disks: vec![Vec::new(); total],
            retired: vec![None; total],
            read_ready: vec![Vec::new(); total],
            snapshot_needed: vec![Vec::new(); total],
            now: Timestamp(0),
        };
        c.drain();
        c
    }

    fn drain(&mut self) {
        let mut queue: VecDeque<(usize, Input)> = VecDeque::new();
        loop {
            for i in 0..self.replicas.len() {
                let Some(r) = self.replicas[i].as_mut() else { continue };
                let mut out = Vec::new();
                r.poll_actions(&mut out);
                for a in out {
                    match a {
                        Action::Send(o) => {
                            let to = o.to.0 as usize;
                            if to < self.replicas.len() && self.replicas[to].is_some() {
                                queue.push_back((to, Input::Message(o.bytes)));
                            }
                        }
                        Action::Persist { id, records, then, replace } => {
                            if replace {
                                self.disks[i].clear();
                            }
                            self.disks[i].extend(records);
                            for o in then {
                                let to = o.to.0 as usize;
                                if to < self.replicas.len() && self.replicas[to].is_some() {
                                    queue.push_back((to, Input::Message(o.bytes)));
                                }
                            }
                            queue.push_back((i, Input::Persisted(id)));
                        }
                        Action::Retired { epoch } => self.retired[i] = Some(epoch),
                        Action::ReadReady { id, slot } => self.read_ready[i].push((id, slot)),
                        Action::SnapshotNeeded { peer, through } => {
                            self.snapshot_needed[i].push((peer, through));
                        }
                        _ => {}
                    }
                }
            }
            let Some((to, input)) = queue.pop_front() else { break };
            if let Some(r) = self.replicas[to].as_mut() {
                r.handle(self.now, input).unwrap();
            }
        }
    }

    fn tick(&mut self, step_ms: u64, count: u64) {
        for _ in 0..count {
            self.now = self.now.after(step_ms * MS);
            for r in self.replicas.iter_mut().flatten() {
                r.handle_timeout(self.now).unwrap();
            }
            self.drain();
        }
    }

    fn r(&self, i: usize) -> &Replica {
        self.replicas[i].as_ref().unwrap()
    }

    fn r_mut(&mut self, i: usize) -> &mut Replica {
        self.replicas[i].as_mut().unwrap()
    }
}

#[test]
fn observer_follows_the_log_indefinitely_and_serves_linearizable_reads() {
    let mut c = MiniCluster::new(&[0, 1, 2], &[3], 50);
    assert!(!c.r(3).is_member());
    assert!(c.r(0).is_member());

    // Write several batches across time; observer 3 catches up via follow polling without voting.
    for round in 0..5u32 {
        let now = c.now;
        c.r_mut(0).propose(now, Bytes::from(alloc::format!("v{round}"))).unwrap();
        c.drain();
        c.tick(50, 2);
        assert_eq!(c.r(3).commit_index(), c.r(0).commit_index());
        assert_eq!(c.r(3).emitted_index(), c.r(0).emitted_index());
        assert_eq!(c.r(3).metrics().live_registers, 0, "observer never holds recorder registers");
        assert!(c.r(3).registers.is_empty());
    }

    // Write on leader 0 without ticking follow, then issue a linearizable read on observer 3:
    // ReadReply carries env.commit_index = committed + 1, so observer pulls immediately without
    // waiting for retransmit_interval.
    let before = c.r(0).commit_index();
    let now = c.now;
    c.r_mut(0).propose(now, Bytes::from_static(b"fresh")).unwrap();
    c.drain();
    assert_eq!(c.r(0).commit_index(), before + 1);
    assert_eq!(c.r(3).commit_index(), before, "observer has not polled yet");

    let read = c.r_mut(3).read_barrier(now).unwrap();
    c.drain();
    assert!(
        c.read_ready[3].iter().any(|&(id, slot)| id == read && slot.0 >= before),
        "observer must finish single-slot lag read without waiting for a timer: {:?}",
        c.read_ready[3]
    );
    assert_eq!(c.r(3).commit_index(), before + 1);
    assert_eq!(c.retired[3], None);
}

#[test]
fn observer_does_not_retire_on_reconfiguration_and_follows_new_members() {
    let mut c = MiniCluster::new(&[0, 1, 2], &[3, 4], 50);
    let now = c.now;
    c.r_mut(0).propose(now, Bytes::from_static(b"init")).unwrap();
    c.drain();
    c.tick(50, 2);
    assert_eq!(c.r(3).commit_index(), c.r(0).commit_index());

    // Replace member 0 with member 4; replica 3 remains a permanent observer.
    let now = c.now;
    c.r_mut(1)
        .propose_reconfiguration(
            now,
            vec![NodeId(1), NodeId(2), NodeId(4)],
            vec![NodeId(1), NodeId(2), NodeId(4)],
        )
        .unwrap();
    c.drain();
    for k in 0..70u32 {
        let now = c.now;
        c.r_mut(1).propose(now, Bytes::from(alloc::format!("w{k}"))).unwrap();
        c.drain();
    }
    c.tick(50, 20);

    // Member 0 (which WAS a voting member) retires; observer 3 (never a member) does NOT.
    assert_eq!(c.retired[0], Some(1), "removed voting member 0 must retire");
    assert_eq!(c.retired[3], None, "observer 3 was never a member and must not retire");
    assert!(c.r(4).is_member(), "joiner 4 became a voting member");
    assert!(!c.r(3).is_member(), "observer 3 is still a non-voting observer");

    // Shut down retired member 0; observer 3 switches to following {1, 2, 4} and serves reads.
    c.replicas[0] = None;
    let now = c.now;
    c.r_mut(4).propose(now, Bytes::from_static(b"after-reconfig")).unwrap();
    c.drain();
    c.tick(50, 4);
    assert_eq!(c.r(3).commit_index(), c.r(1).commit_index());

    let now = c.now;
    let read = c.r_mut(3).read_barrier(now).unwrap();
    c.drain();
    assert!(
        c.read_ready[3].iter().any(|&(id, slot)| id == read && slot.0 + 1 == c.r(1).commit_index()),
        "observer serves linearizable reads in the new epoch: {:?}",
        c.read_ready[3]
    );
    assert_eq!(c.retired[3], None);
}

#[test]
fn observer_rotates_away_from_a_crashed_voter_and_keeps_following() {
    let mut c = MiniCluster::new(&[0, 1, 2], &[3], 50);
    let now = c.now;
    c.r_mut(0).propose(now, Bytes::from_static(b"a")).unwrap();
    c.drain();
    c.tick(50, 2);
    assert_eq!(c.r(3).commit_index(), c.r(0).commit_index());

    // Crash member 0 (likely the observer's preferred follow peer).
    c.replicas[0] = None;
    let now = c.now;
    c.r_mut(1).propose(now, Bytes::from_static(b"b")).unwrap();
    c.drain();
    // Advance past hedge + follow rotation so {1, 2} decide and observer 3 rotates to a live peer.
    c.tick(100, 10);
    assert_eq!(c.r(3).commit_index(), c.r(1).commit_index());
    assert!(c.r(3).commit_index() >= 2);
}

#[test]
fn observer_installs_snapshot_when_voters_compact_and_keeps_following() {
    let mut c = MiniCluster::new(&[0, 1, 2], &[], 50);
    for k in 0..10u32 {
        let now = c.now;
        c.r_mut(0).propose(now, Bytes::from(alloc::format!("s{k}"))).unwrap();
        c.drain();
    }
    let snap_slot = Slot(c.r(0).emitted_index() - 1);
    let core_state = c.r(0).core_state();
    for i in 0..3 {
        let now = c.now;
        c.r_mut(i).compact(now, snap_slot, core_state.clone()).unwrap();
    }
    c.drain();

    // Start a brand-new observer after all voters compacted slots 0..=snap_slot.
    let mut cfg = Config::new([3; 16], NodeId(3), vec![NodeId(0), NodeId(1), NodeId(2)], [4; 32]);
    cfg.follow_interval = 50 * MS;
    c.replicas.push(Some(Replica::new(cfg, [], c.now).unwrap()));
    c.disks.push(Vec::new());
    c.retired.push(None);
    c.read_ready.push(Vec::new());
    c.snapshot_needed.push(Vec::new());
    c.drain();

    assert!(!c.snapshot_needed[3].is_empty(), "observer behind compaction must request a snapshot");
    let now = c.now;
    let installed = c.r_mut(3).install_snapshot(now, snap_slot, core_state).unwrap();
    assert!(installed);
    c.drain();
    assert_eq!(c.r(3).emitted_index(), snap_slot.0 + 1);

    // Subsequent writes are followed normally after snapshot installation.
    let now = c.now;
    c.r_mut(0).propose(now, Bytes::from_static(b"post-snap")).unwrap();
    c.drain();
    c.tick(50, 2);
    assert_eq!(c.r(3).commit_index(), c.r(0).commit_index());
    let now = c.now;
    let read = c.r_mut(3).read_barrier(now).unwrap();
    c.drain();
    assert!(c.read_ready[3].iter().any(|&(id, _)| id == read));
}

#[test]
fn promoted_observer_stops_polling_and_retires_if_later_removed() {
    let mut c = MiniCluster::new(&[0, 1, 2], &[3], 50);
    for k in 0..5u32 {
        let now = c.now;
        c.r_mut(0).propose(now, Bytes::from(alloc::format!("pre{k}"))).unwrap();
        c.drain();
    }
    c.tick(50, 2);
    assert_eq!(c.r(3).commit_index(), 5, "observer caught up before joining");

    // Epoch 1: promote observer 3 into {0, 1, 2, 3}.
    let now = c.now;
    c.r_mut(0)
        .propose_reconfiguration(
            now,
            vec![NodeId(0), NodeId(1), NodeId(2), NodeId(3)],
            vec![NodeId(0), NodeId(1), NodeId(2), NodeId(3)],
        )
        .unwrap();
    c.drain();
    for k in 0..70u32 {
        let now = c.now;
        c.r_mut(0).propose(now, Bytes::from(alloc::format!("e1-{k}"))).unwrap();
        c.drain();
    }
    assert!(c.r(3).is_member());
    assert_eq!(c.r(3).follow.deadline(c.r(3).outside(), c.r(3).cfg.follow_interval), None);
    assert_eq!(c.retired[3], None);

    // Epoch 2: remove 3 -> since it WAS a voting member in epoch 1, it MUST retire.
    let now = c.now;
    c.r_mut(0)
        .propose_reconfiguration(
            now,
            vec![NodeId(0), NodeId(1), NodeId(2)],
            vec![NodeId(0), NodeId(1), NodeId(2)],
        )
        .unwrap();
    c.drain();
    for k in 0..70u32 {
        let now = c.now;
        c.r_mut(0).propose(now, Bytes::from(alloc::format!("e2-{k}"))).unwrap();
        c.drain();
    }
    c.tick(50, 20);
    assert_eq!(c.retired[3], Some(2), "promoted-then-removed replica must retire");
}

#[test]
fn restarted_observer_stays_non_retiring_and_resumes_following() {
    let mut c = MiniCluster::new(&[0, 1, 2], &[3], 50);
    let now = c.now;
    c.r_mut(0).propose(now, Bytes::from_static(b"before-restart")).unwrap();
    c.drain();
    c.tick(50, 2);
    assert_eq!(c.r(3).commit_index(), 1);

    // Restart observer 3 from its durable records.
    let mut cfg = Config::new([3; 16], NodeId(3), vec![NodeId(0), NodeId(1), NodeId(2)], [99; 32]);
    cfg.follow_interval = 50 * MS;
    c.replicas[3] = Some(Replica::new(cfg, c.disks[3].clone(), c.now).unwrap());
    c.drain();

    // Leader-order reconfiguration on voters: restarted observer must not retire.
    let now = c.now;
    c.r_mut(0).propose_leader_order(now, vec![NodeId(1), NodeId(2), NodeId(0)]).unwrap();
    c.drain();
    for k in 0..70u32 {
        let now = c.now;
        c.r_mut(0).propose(now, Bytes::from(alloc::format!("r{k}"))).unwrap();
        c.drain();
    }
    c.tick(50, 20);
    assert_eq!(c.r(3).commit_index(), c.r(1).commit_index());
    assert_eq!(c.retired[3], None, "restarted observer must not retire on leader-order change");
}

#[test]
fn observer_read_barrier_completes_on_empty_cluster() {
    let mut c = MiniCluster::new(&[0, 1, 2], &[3], 50);
    let now = c.now;
    let read = c.r_mut(3).read_barrier(now).unwrap();
    c.drain();
    c.tick(200, 15);
    assert!(
        c.read_ready[3].iter().any(|&(id, _)| id == read),
        "read on fresh empty cluster must still finish: {:?}",
        c.read_ready[3]
    );
    // Verify no stale CatchUp bursts leaked before fencing.
    let mut cfg = Config::new([3; 16], NodeId(3), vec![NodeId(0), NodeId(1), NodeId(2)], [7; 32]);
    cfg.follow_interval = 50 * MS;
    let mut r = Replica::new(cfg, [], Timestamp(0)).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    // Initial CatchUp requests are gated behind the Incarnation persist.
    let catchups = out
        .iter()
        .filter_map(|a| match a {
            Action::Persist { then, .. } => Some(then.iter()),
            _ => None,
        })
        .flatten()
        .filter(|o| {
            matches!(Message::decode(&o.bytes, &r_wire()), Ok(Message { body: Body::CatchUp { .. }, .. }))
        })
        .count();
    assert_eq!(catchups, 3, "initial follow probes every voting member behind Incarnation persist");
}

#[test]
fn observer_follows_large_referenced_batches_and_serves_reads() {
    let mut c = MiniCluster::new(&[0, 1, 2], &[3], 50);
    // Use a 4 KiB payload proposed at follower 1 so it travels by reference (`Data` + `Ref`)
    // between voting members (observer 3 never receives the origin's `Data` broadcast).
    let payload = Bytes::from(vec![b'z'; 4096]);
    let now = c.now;
    c.r_mut(1).propose(now, payload).unwrap();
    c.drain();
    c.tick(50, 3);
    assert_eq!(c.r(3).commit_index(), c.r(0).commit_index());
    assert_eq!(c.r(3).emitted_index(), c.r(0).emitted_index());
    let now = c.now;
    let read = c.r_mut(3).read_barrier(now).unwrap();
    c.drain();
    assert!(c.read_ready[3].iter().any(|&(id, _)| id == read));
}

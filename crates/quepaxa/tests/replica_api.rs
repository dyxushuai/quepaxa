//! Contract tests of the public `Replica` API on a single-member cluster.

use bytes::Bytes;
use quepaxa::{Action, Config, DropReason, Error, Input, NodeId, PersistId, Replica, Slot, Timestamp};

const T0: Timestamp = Timestamp(0);

fn config(members: &[u32]) -> Config {
    Config::new([9; 16], NodeId(members[0]), members.iter().copied().map(NodeId).collect(), [1; 32])
}

fn drain(r: &mut Replica) -> Vec<Action> {
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    out
}

/// Acknowledges every persist until the replica goes quiet; returns all other actions
/// and the durable records written.
fn settle(r: &mut Replica, durable: &mut Vec<Bytes>) -> Vec<Action> {
    let mut seen = Vec::new();
    let mut pending = drain(r);
    while !pending.is_empty() {
        let mut acks = Vec::new();
        for a in pending {
            match a {
                Action::Persist { id, records, replace, .. } => {
                    if replace {
                        durable.clear();
                    }
                    durable.extend(records);
                    acks.push(id);
                }
                other => seen.push(other),
            }
        }
        pending = Vec::new();
        for id in acks {
            r.handle(T0, Input::Persisted(id)).unwrap();
            pending.extend(drain(r));
        }
    }
    seen
}

#[test]
fn fresh_replica_first_persists_its_incarnation() {
    let mut r = Replica::new(config(&[0]), [], T0).unwrap();
    let actions = drain(&mut r);
    assert!(
        matches!(&actions[..], [Action::Persist { id: PersistId(0), records, then, replace: false }] if records.len() == 1 && then.is_empty())
    );
}

#[test]
fn single_member_commits_a_proposal_only_after_it_is_durable() {
    let mut r = Replica::new(config(&[0]), [], T0).unwrap();
    let mut disk = Vec::new();
    settle(&mut r, &mut disk);
    let id = r.propose(T0, Bytes::from_static(b"x")).unwrap();
    let first = drain(&mut r);
    assert!(
        first.iter().all(|a| matches!(a, Action::Persist { .. })),
        "nothing commits before a write lands: {first:?}"
    );
    let mut actions = Vec::new();
    for a in first {
        if let Action::Persist { id, records, .. } = a {
            disk.extend(records);
            r.handle(T0, Input::Persisted(id)).unwrap();
            actions.extend(settle(&mut r, &mut disk));
        }
    }
    assert!(actions.iter().any(|a| matches!(a, Action::Commit { slot: Slot(0), entries } if entries.len() == 1 && entries[0].payload == "x")));
    assert!(actions.contains(&Action::Committed { id, slot: Slot(0) }));
}

#[test]
fn restart_replays_committed_slots_from_durable_records() {
    let mut disk = Vec::new();
    let mut r = Replica::new(config(&[0]), [], T0).unwrap();
    settle(&mut r, &mut disk);
    r.propose(T0, Bytes::from_static(b"x")).unwrap();
    settle(&mut r, &mut disk);
    let mut r2 = Replica::new(config(&[0]), disk.clone(), T0).unwrap();
    let replay = drain(&mut r2);
    assert!(matches!(&replay[0], Action::Commit { slot: Slot(0), entries } if entries[0].payload == "x"));
    assert_eq!(r2.commit_index(), 1);
}

#[test]
fn corrupt_durable_record_is_an_error_not_skipped() {
    let mut disk = Vec::new();
    let mut r = Replica::new(config(&[0]), [], T0).unwrap();
    settle(&mut r, &mut disk);
    let mut bad = disk[0].to_vec();
    bad[3] ^= 0xFF;
    let err = Replica::new(config(&[0]), [Bytes::from(bad)], T0).err();
    assert_eq!(err, Some(Error::CorruptRecord { index: 0 }));
}

#[test]
fn calling_again_before_draining_is_rejected() {
    let mut r = Replica::new(config(&[0]), [], T0).unwrap();
    assert_eq!(r.handle_timeout(T0), Err(Error::NotDrained));
}

#[test]
fn unknown_persist_id_is_rejected() {
    let mut r = Replica::new(config(&[0]), [], T0).unwrap();
    drain(&mut r);
    assert_eq!(r.handle(T0, Input::Persisted(PersistId(5))), Err(Error::UnknownPersistId));
}

#[test]
fn storage_failure_fences_and_drops_outstanding_proposals() {
    let mut r = Replica::new(config(&[0, 1, 2]), [], T0).unwrap();
    let mut disk = Vec::new();
    settle(&mut r, &mut disk);
    let id = r.propose(T0, Bytes::from_static(b"x")).unwrap();
    drain(&mut r);
    r.handle(T0, Input::StorageFailed(PersistId(1))).unwrap();
    assert_eq!(drain(&mut r), vec![Action::Dropped { id, reason: DropReason::Fenced }]);
    assert_eq!(r.propose(T0, Bytes::new()), Err(Error::Fenced));
    assert_eq!(r.next_deadline(), None);
}

#[test]
fn backpressure_and_size_limits_are_enforced() {
    let mut cfg = config(&[0, 1, 2]);
    cfg.max_outstanding_proposals = 1;
    cfg.max_batch_bytes = 64;
    let mut r = Replica::new(cfg, [], T0).unwrap();
    drain(&mut r);
    assert_eq!(r.propose(T0, Bytes::from(vec![0; 64])), Err(Error::PayloadTooLarge));
    r.propose(T0, Bytes::new()).unwrap();
    drain(&mut r);
    assert_eq!(r.propose(T0, Bytes::new()), Err(Error::Busy));
    // The smallest valid batch still fits a read barrier.
    let mut cfg = config(&[0, 1, 2]);
    cfg.max_batch_bytes = 64;
    let mut r = Replica::new(cfg, [], T0).unwrap();
    drain(&mut r);
    r.read_barrier(T0).unwrap();
}

#[test]
fn invalid_configs_are_rejected() {
    let mut dup = config(&[0, 0]);
    dup.id = NodeId(0);
    assert!(matches!(Replica::new(dup, [], T0), Err(Error::InvalidConfig(_))));
    // A replica outside the configured members is a joining replica, not an error.
    let mut crowd = config(&[0]);
    crowd.members = (0..1025).map(NodeId).collect();
    assert!(matches!(Replica::new(crowd, [], T0), Err(Error::InvalidConfig(_))));
    let mut nobody = config(&[0]);
    nobody.members.clear();
    assert!(matches!(Replica::new(nobody, [], T0), Err(Error::InvalidConfig(_))));
    let mut joiner = config(&[0, 1]);
    joiner.id = NodeId(7);
    assert!(Replica::new(joiner, [], T0).is_ok());
}

#[test]
fn installing_a_stale_snapshot_reports_it_was_ignored() {
    let mut r = Replica::new(config(&[0]), [], T0).unwrap();
    let mut disk = Vec::new();
    settle(&mut r, &mut disk);
    let early = r.core_state(); // covers nothing yet
    for p in [b"a", b"b"] {
        r.propose(T0, Bytes::from_static(p)).unwrap();
        settle(&mut r, &mut disk);
    }
    assert_eq!(r.emitted_index(), 2);
    let state = r.core_state();
    r.compact(T0, Slot(1), state.clone()).unwrap();
    settle(&mut r, &mut disk);
    assert_eq!(r.install_snapshot(T0, Slot(1), state), Ok(false));
    assert_eq!(r.install_snapshot(T0, Slot(0), early), Err(Error::InvalidCoreState));
}

#[test]
fn compaction_rewrites_durable_state_once_and_restarts_from_the_snapshot() {
    let mut r = Replica::new(config(&[0]), [], T0).unwrap();
    let mut disk = Vec::new();
    settle(&mut r, &mut disk);
    let mut states = Vec::new();
    for p in [b"a", b"b", b"c"] {
        r.propose(T0, Bytes::from_static(p)).unwrap();
        settle(&mut r, &mut disk);
        states.push(r.core_state());
    }
    let before = disk.len();
    let state = states[2].clone();
    assert_eq!(r.compact(T0, Slot(1), state.clone()), Err(Error::InvalidCoreState));
    // An older snapshot (taken after slot 1) may still compact, then the newest one.
    assert_eq!(r.compact(T0, Slot(1), states[1].clone()), Ok(()));
    drain_and_ack(&mut r, &mut disk);
    assert_eq!(r.compact(T0, Slot(2), state.clone()), Ok(()));
    let replaced =
        |a: &[Action]| a.iter().filter(|a| matches!(a, Action::Persist { replace: true, .. })).count();
    assert_eq!(replaced(&drain_and_ack(&mut r, &mut disk)), 1);
    assert!(disk.len() < before, "{} records before, {} after", before, disk.len());
    r.compact(T0, Slot(2), state.clone()).unwrap();
    assert!(drain(&mut r).is_empty(), "a repeated compaction is ignored");
    // Restarted from the checkpoint, the replica re-emits nothing the snapshot covers.
    let mut r = Replica::new(config(&[0]), disk.clone(), T0).unwrap();
    let commits =
        settle(&mut r, &mut disk).into_iter().filter(|a| matches!(a, Action::Commit { .. })).count();
    assert_eq!((commits, r.emitted_index()), (0, 3));
    assert_eq!(r.install_snapshot(T0, Slot(2), state), Ok(false));
}

/// Like [`settle`], but also returns the persists.
fn drain_and_ack(r: &mut Replica, durable: &mut Vec<Bytes>) -> Vec<Action> {
    let actions = drain(r);
    for a in &actions {
        if let Action::Persist { id, records, replace, .. } = a {
            if *replace {
                durable.clear();
            }
            durable.extend(records.iter().cloned());
            r.handle(T0, Input::Persisted(*id)).unwrap();
        }
    }
    actions
}

#[test]
fn configs_that_would_misbehave_at_runtime_are_rejected() {
    let bad: [fn(&mut Config); 8] = [
        |c| (c.hedge_min, c.hedge_max) = (10, 5),
        |c| (c.hedge_min, c.hedge_max) = (0, 0),
        |c| c.max_in_flight_slots = 65,
        |c| c.max_in_flight_slots = 0,
        |c| c.max_message_bytes = 2 * c.max_batch_bytes,
        |c| c.max_batch_bytes = 63,
        |c| c.max_outstanding_proposals = 0,
        |c| c.retransmit_interval = 0,
    ];
    for tweak in bad {
        let mut cfg = config(&[0, 1, 2]);
        tweak(&mut cfg);
        assert!(matches!(Replica::new(cfg, [], T0), Err(Error::InvalidConfig(_))));
    }
}

#[test]
fn garbage_network_input_is_counted_and_ignored() {
    let mut r = Replica::new(config(&[0, 1, 2]), [], T0).unwrap();
    drain(&mut r);
    r.handle(T0, Input::Message(Bytes::from_static(b"not a message"))).unwrap();
    assert!(drain(&mut r).is_empty());
    assert_eq!(r.metrics().messages_rejected, 1);
}

#[test]
fn read_barrier_reports_a_slot_after_earlier_commits() {
    let mut r = Replica::new(config(&[0]), [], T0).unwrap();
    let mut disk = Vec::new();
    settle(&mut r, &mut disk);
    let write = r.propose(T0, Bytes::from_static(b"x")).unwrap();
    let mut seen = settle(&mut r, &mut disk);
    let read = r.read_barrier(T0).unwrap();
    seen.extend(settle(&mut r, &mut disk));
    let ws = seen
        .iter()
        .find_map(|a| match a {
            Action::Committed { id, slot } if *id == write => Some(*slot),
            _ => None,
        })
        .expect("write committed");
    assert!(seen.iter().any(|a| matches!(a, Action::ReadReady { id, slot } if *id == read && *slot > ws)));
    assert!(
        !seen
            .iter()
            .any(|a| matches!(a, Action::Commit { entries, .. } if entries.iter().any(|e| e.id == read))),
        "barriers never reach the application"
    );
}

/// Delivers every message and acknowledges every persist at once until all replicas are quiet.
fn run_cluster(rs: &mut [Replica]) {
    run_cluster_on_disk(rs, &mut vec![Vec::new(); rs.len()]);
}

/// Like [`run_cluster`], keeping each replica's durable records.
fn run_cluster_on_disk(rs: &mut [Replica], disks: &mut [Vec<Bytes>]) {
    let mut queue = std::collections::VecDeque::new();
    loop {
        for (i, r) in rs.iter_mut().enumerate() {
            for a in drain(r) {
                match a {
                    Action::Send(o) => queue.push_back((o.to.0 as usize, Input::Message(o.bytes))),
                    Action::Persist { id, then, records, replace } => {
                        if replace {
                            disks[i].clear();
                        }
                        disks[i].extend(records);
                        queue.extend(then.into_iter().map(|o| (o.to.0 as usize, Input::Message(o.bytes))));
                        queue.push_back((i, Input::Persisted(id)));
                    }
                    _ => {}
                }
            }
        }
        let Some((to, input)) = queue.pop_front() else { return };
        rs[to].handle(T0, input).unwrap();
    }
}

fn ordered(id: u32, order: &[u32]) -> Replica {
    let mut cfg = config(&[0, 1, 2]);
    cfg.id = NodeId(id);
    cfg.leader_order = order.iter().copied().map(NodeId).collect();
    Replica::new(cfg, [], T0).unwrap()
}

#[test]
fn the_first_member_of_the_leader_order_leads() {
    let mut rs = [ordered(0, &[2, 0, 1]), ordered(1, &[2, 0, 1]), ordered(2, &[2, 0, 1])];
    run_cluster(&mut rs);
    rs[0].propose(T0, Bytes::from_static(b"x")).unwrap();
    run_cluster(&mut rs);
    assert_eq!(rs[0].metrics().forwarded, 1, "a follower forwards to the leader");
    assert_eq!(rs[2].metrics().fast_path_decisions, 1);
    assert!(rs.iter().all(|r| r.commit_index() == 1));
}

#[test]
fn members_with_different_leader_orders_ignore_each_other() {
    // Listing the members by id is the default order: those still talk.
    let mut same = [ordered(0, &[0, 1, 2]), ordered(1, &[]), ordered(2, &[])];
    run_cluster(&mut same);
    same[0].propose(T0, Bytes::from_static(b"x")).unwrap();
    run_cluster(&mut same);
    assert!(same.iter().all(|r| r.commit_index() == 1 && r.metrics().messages_rejected == 0));
    let mut split = [ordered(0, &[]), ordered(1, &[]), ordered(2, &[1, 0, 2])];
    run_cluster(&mut split);
    split[0].propose(T0, Bytes::from_static(b"y")).unwrap();
    run_cluster(&mut split);
    assert!(split[2].metrics().messages_rejected > 0);
    assert_eq!(split[2].commit_index(), 0, "the misconfigured member takes no part");
    assert_eq!(split[0].commit_index(), 1, "the others still form a majority");
}

#[test]
fn state_built_under_another_leader_order_is_refused() {
    let mut rs = [ordered(0, &[]), ordered(1, &[]), ordered(2, &[])];
    let mut disks = vec![Vec::new(); 3];
    run_cluster_on_disk(&mut rs, &mut disks);
    rs[0].propose(T0, Bytes::from_static(b"x")).unwrap();
    run_cluster_on_disk(&mut rs, &mut disks);
    let state = rs[0].core_state();
    rs[0].compact(T0, Slot(0), state.clone()).unwrap();
    run_cluster_on_disk(&mut rs, &mut disks);
    let mut cfg = config(&[0, 1, 2]);
    cfg.leader_order = vec![NodeId(2), NodeId(0), NodeId(1)];
    assert!(matches!(Replica::new(cfg.clone(), disks[0].clone(), T0), Err(Error::InvalidConfig(_))));
    // The same order restarts fine, and a fresh replica under the new order refuses the
    // snapshot as well.
    assert!(Replica::new(config(&[0, 1, 2]), disks[0].clone(), T0).is_ok());
    let mut fresh = Replica::new(cfg, [], T0).unwrap();
    drain(&mut fresh);
    assert_eq!(fresh.install_snapshot(T0, Slot(0), state), Err(Error::InvalidCoreState));
}

#[test]
fn a_leader_order_must_list_every_member_once() {
    for order in [&[0, 1][..], &[0, 1, 1], &[0, 1, 2, 3], &[0, 1, 7]] {
        let mut cfg = config(&[0, 1, 2]);
        cfg.leader_order = order.iter().copied().map(NodeId).collect();
        assert!(matches!(Replica::new(cfg, [], T0), Err(Error::InvalidConfig(_))), "{order:?}");
    }
}

/// Like [`run_cluster`], but replicas in `down` neither send nor receive.
fn run_without(rs: &mut [Replica], down: &[usize]) -> Vec<(usize, Action)> {
    let mut queue = std::collections::VecDeque::new();
    let mut seen = Vec::new();
    loop {
        for (i, r) in rs.iter_mut().enumerate() {
            for a in drain(r) {
                match a {
                    Action::Send(o) => queue.push_back((o.to.0 as usize, Input::Message(o.bytes))),
                    Action::Persist { id, then, .. } => {
                        queue.extend(then.into_iter().map(|o| (o.to.0 as usize, Input::Message(o.bytes))));
                        queue.push_back((i, Input::Persisted(id)));
                    }
                    other => seen.push((i, other)),
                }
            }
        }
        let Some((to, input)) = queue.pop_front() else { return seen };
        if down.contains(&to) && !matches!(input, Input::Persisted(_)) {
            continue;
        }
        rs[to].handle(T0, input).unwrap();
    }
}

#[test]
fn a_member_is_replaced_and_the_new_majority_commits_without_it() {
    // Replica 3 joins with the original configuration; replica 0 leaves.
    let mut rs = [ordered(0, &[]), ordered(1, &[]), ordered(2, &[]), ordered(3, &[])];
    let mut seen = run_without(&mut rs, &[]);
    let id = rs[0].propose_reconfiguration(T0, vec![NodeId(1), NodeId(2), NodeId(3)], vec![]).unwrap();
    // One change at a time.
    assert_eq!(
        rs[0].propose_reconfiguration(T0, vec![NodeId(0), NodeId(1), NodeId(2)], vec![]),
        Err(Error::ReconfigurationPending)
    );
    seen.extend(run_without(&mut rs, &[]));
    assert!(seen.iter().any(|(i, a)| *i == 0 && matches!(a, Action::Committed { id: c, .. } if *c == id)));
    // Writes carry on through the window (which also fills by itself).
    for k in 0..70u32 {
        rs[1].propose(T0, Bytes::from(format!("w{k}"))).unwrap();
        seen.extend(run_without(&mut rs, &[]));
    }
    let reconfigured: Vec<usize> = seen
        .iter()
        .filter_map(|(i, a)| match a {
            Action::Reconfigured { epoch: 1, members, .. }
                if members == &[NodeId(1), NodeId(2), NodeId(3)] =>
            {
                Some(*i)
            }
            _ => None,
        })
        .collect();
    assert_eq!(reconfigured, [0, 1, 2, 3], "every replica, the joiner included, applies the change once");
    // The removed replica asks the new members for their progress on its retransmit timer.
    for r in &mut rs {
        r.handle_timeout(Timestamp(1_000_000_000)).unwrap();
    }
    seen.extend(run_without(&mut rs, &[]));
    let retired: Vec<(usize, u64)> = seen
        .iter()
        .filter_map(|(i, a)| match a {
            Action::Retired { epoch } => Some((*i, *epoch)),
            _ => None,
        })
        .collect();
    // The joiner is not a configured member either, but epoch 0 removes nobody.
    assert_eq!(retired, [(0, 1)], "only the removed replica retires, once, from the epoch that removed it");
    let before = rs[1].commit_index();
    assert!(rs.iter().all(|r| r.commit_index() == before), "the joiner caught up");
    // Without replica 0 the new members still form a majority, on the fast path.
    let fast = rs[1].metrics().fast_path_decisions;
    rs[3].propose(T0, Bytes::from_static(b"after")).unwrap();
    run_without(&mut rs, &[0]);
    assert_eq!(rs[1].commit_index(), before + 1);
    assert_eq!(rs[3].commit_index(), before + 1);
    assert_eq!(rs[1].metrics().fast_path_decisions, fast + 1, "replica 1 now leads");
}

#[test]
fn removed_majority_retires_only_after_the_new_members_caught_up() {
    // Replicas 0 and 1 leave, 3 and 4 join: once the window has passed, only replica 2 of
    // the old members remains, so 0 and 1 must wait for the joiners to learn the old slots.
    let mut rs: Vec<Replica> = (0..5).map(|i| ordered(i, &[])).collect();
    run_without(&mut rs, &[3, 4]);
    let joiners = vec![NodeId(2), NodeId(3), NodeId(4)];
    rs[0].propose_reconfiguration(T0, joiners, vec![]).unwrap();
    let mut seen = run_without(&mut rs, &[3, 4]);
    for k in 0..70u32 {
        rs[0].propose(T0, Bytes::from(format!("w{k}"))).unwrap();
        seen.extend(run_without(&mut rs, &[3, 4]));
    }
    let from = seen
        .iter()
        .find_map(|(_, a)| match a {
            Action::Reconfigured { epoch: 1, from, .. } => Some(from.0),
            _ => None,
        })
        .expect("the change was applied");
    assert!(rs[0].commit_index() >= from, "the old members decided the whole window");
    assert_eq!(rs[3].commit_index(), 0);
    let retired = |seen: &[(usize, Action)]| {
        seen.iter()
            .filter(|(_, a)| matches!(a, Action::Retired { epoch: 1 }))
            .map(|(i, _)| *i)
            .collect::<Vec<_>>()
    };
    assert_eq!(retired(&seen), [0usize; 0], "the joiners know none of the old slots yet");
    // The joiners come back; the removed replicas' probes tell them how far the log goes,
    // they catch up, and the next probes report their durable progress.
    let mut t = 0;
    while retired(&seen).len() < 2 {
        t += 1;
        assert!(
            t < 20,
            "retirement never reported: {:?}",
            rs.iter().map(Replica::commit_index).collect::<Vec<_>>()
        );
        let now = Timestamp(t * 1_000_000_000);
        for r in &mut rs {
            r.handle_timeout(now).unwrap();
        }
        seen.extend(run_without(&mut rs, &[]));
    }
    let mut both = retired(&seen);
    both.sort_unstable();
    assert_eq!(both, [0, 1]);
    assert!(rs[3].commit_index() >= from && rs[4].commit_index() >= from, "joiners caught up first");
}

#[test]
fn half_of_an_even_new_membership_is_not_a_majority() {
    // Replica 0 leaves; the new members are 1 and 2 (old, up to date) and 3 and 4 (joining,
    // away). Two of four caught up is half, not a majority: replica 0 must keep waiting,
    // however often it asks.
    let mut rs: Vec<Replica> = (0..5).map(|i| ordered(i, &[])).collect();
    run_without(&mut rs, &[3, 4]);
    rs[0].propose_reconfiguration(T0, vec![NodeId(1), NodeId(2), NodeId(3), NodeId(4)], vec![]).unwrap();
    let mut seen = run_without(&mut rs, &[3, 4]);
    for k in 0..70u32 {
        rs[1].propose(T0, Bytes::from(format!("w{k}"))).unwrap();
        seen.extend(run_without(&mut rs, &[3, 4]));
    }
    let retired = |seen: &[(usize, Action)]| seen.iter().any(|(_, a)| matches!(a, Action::Retired { .. }));
    for t in 1..20 {
        for r in &mut rs[..3] {
            r.handle_timeout(Timestamp(t * 100_000_000)).unwrap();
        }
        seen.extend(run_without(&mut rs, &[3, 4]));
    }
    let from = seen
        .iter()
        .find_map(|(_, a)| match a {
            Action::Reconfigured { epoch: 1, from, .. } => Some(from.0),
            _ => None,
        })
        .expect("the change was applied");
    // The new epoch needs three of four, so the old members stop at its first slot.
    assert!(
        rs[1].commit_index() >= from && rs[2].commit_index() >= from,
        "the old members finished the window"
    );
    assert!(!retired(&seen), "replica 0 retired with only two of four new members caught up");
    // A third catching up is a majority.
    for t in 20..60 {
        for r in &mut rs[..4] {
            r.handle_timeout(Timestamp(t * 100_000_000)).unwrap();
        }
        seen.extend(run_without(&mut rs, &[4]));
    }
    assert!(seen.iter().any(|(i, a)| *i == 0 && matches!(a, Action::Retired { epoch: 1 })));
}

#[test]
fn a_removed_replica_added_back_while_away_does_not_retire_on_a_stale_view() {
    // Small messages, so catching up takes many pages of a few slots each.
    let small = |id: u32| {
        let mut cfg = config(&[0, 1, 2]);
        cfg.id = NodeId(id);
        (cfg.max_batch_bytes, cfg.max_message_bytes) = (64, 1152);
        Replica::new(cfg, [], T0).unwrap()
    };
    let mut rs: Vec<Replica> = (0..4).map(small).collect();
    run_without(&mut rs, &[]);
    // Replica 2 learns that it is removed, then goes away before the new members caught up.
    rs[0].propose_reconfiguration(T0, vec![NodeId(0), NodeId(1), NodeId(3)], vec![]).unwrap();
    let mut seen = run_without(&mut rs, &[]);
    let retired =
        |seen: &[(usize, Action)]| seen.iter().any(|(i, a)| *i == 2 && matches!(a, Action::Retired { .. }));
    assert!(!retired(&seen));
    // Meanwhile the window passes and a second change adds replica 2 back.
    let write = |rs: &mut Vec<Replica>, k: u32| {
        rs[0].propose(T0, Bytes::from(format!("w{k}"))).unwrap();
        run_without(rs, &[2])
    };
    for k in 0..70 {
        seen.extend(write(&mut rs, k));
    }
    rs[0].propose_reconfiguration(T0, vec![NodeId(0), NodeId(1), NodeId(2), NodeId(3)], vec![]).unwrap();
    seen.extend(run_without(&mut rs, &[2]));
    for k in 70..140 {
        seen.extend(write(&mut rs, k));
    }
    assert!(seen.iter().any(|(i, a)| *i == 0 && matches!(a, Action::Reconfigured { epoch: 2, .. })));
    // Replica 2 comes back far behind. Members report progress past the epoch it was removed
    // from, but also past what it knows: it must catch up (and find itself added back)
    // instead of retiring.
    let target = rs[0].commit_index();
    let mut t = 0;
    while rs[2].commit_index() < target {
        t += 1;
        assert!(t < 200, "replica 2 did not catch up: {} of {target}", rs[2].commit_index());
        for r in &mut rs {
            r.handle_timeout(Timestamp(t * 100_000_000)).unwrap();
        }
        seen.extend(run_without(&mut rs, &[]));
    }
    assert!(!retired(&seen), "replica 2 retired from epoch 1 although epoch 2 added it back");
}

/// Like [`run_without`], but messages to a replica in `hold` are kept in `held` instead of
/// being delivered.
fn run_holding(rs: &mut [Replica], hold: &[usize], held: &mut Vec<(usize, Input)>) -> Vec<(usize, Action)> {
    let mut queue: std::collections::VecDeque<(usize, Input)> = std::mem::take(held).into();
    held.clear();
    let mut seen = Vec::new();
    loop {
        for (i, r) in rs.iter_mut().enumerate() {
            for a in drain(r) {
                match a {
                    Action::Send(o) => queue.push_back((o.to.0 as usize, Input::Message(o.bytes))),
                    Action::Persist { id, then, .. } => {
                        queue.extend(then.into_iter().map(|o| (o.to.0 as usize, Input::Message(o.bytes))));
                        queue.push_back((i, Input::Persisted(id)));
                    }
                    other => seen.push((i, other)),
                }
            }
        }
        let Some((to, input)) = queue.pop_front() else { return seen };
        if hold.contains(&to) && !matches!(input, Input::Persisted(_)) {
            held.push((to, input));
            continue;
        }
        rs[to].handle(T0, input).unwrap();
    }
}

#[test]
fn a_replica_added_back_after_its_probes_were_answered_does_not_retire() {
    // Replica 2 leaves; it learns every old slot, and the window ends with nothing after it.
    let mut rs: Vec<Replica> = (0..4).map(|i| ordered(i, &[])).collect();
    let mut seen = run_without(&mut rs, &[]);
    rs[0].propose_reconfiguration(T0, vec![NodeId(0), NodeId(1), NodeId(3)], vec![]).unwrap();
    seen.extend(run_without(&mut rs, &[]));
    let from = seen
        .iter()
        .find_map(|(_, a)| match a {
            Action::Reconfigured { epoch: 1, from, .. } => Some(from.0),
            _ => None,
        })
        .expect("the change was applied");
    let mut k = 0;
    while rs[0].commit_index() < from {
        rs[0].propose(T0, Bytes::from(format!("w{k}"))).unwrap();
        seen.extend(run_without(&mut rs, &[]));
        k += 1;
    }
    assert_eq!(rs.iter().map(Replica::commit_index).collect::<Vec<_>>(), [from; 4]);
    // Its probes are answered, but the answers are held back...
    let mut probes = Vec::new();
    rs[2].handle_timeout(Timestamp(1_000_000_000)).unwrap();
    seen.extend(run_holding(&mut rs, &[2], &mut probes));
    assert!(!probes.is_empty(), "replica 2 asked the new members for their progress");
    // ...while a second change adds it back, before it hears of that.
    rs[0].propose_reconfiguration(T0, vec![NodeId(0), NodeId(1), NodeId(2), NodeId(3)], vec![]).unwrap();
    let mut later = Vec::new();
    seen.extend(run_holding(&mut rs, &[2], &mut later));
    assert!(rs[0].commit_index() > from, "the second change is decided");
    // The held answers say a majority caught up and nobody knows more than replica 2: only
    // its own read barrier, ordered after the second change, shows it was added back.
    seen.extend(run_holding(&mut rs, &[], &mut probes));
    seen.extend(run_holding(&mut rs, &[], &mut later));
    for t in 2..20 {
        for r in &mut rs {
            r.handle_timeout(Timestamp(t * 1_000_000_000)).unwrap();
        }
        seen.extend(run_without(&mut rs, &[]));
    }
    assert!(rs[2].commit_index() > from, "replica 2 learned the second change");
    let retired: Vec<_> = seen.iter().filter(|(_, a)| matches!(a, Action::Retired { .. })).collect();
    assert!(retired.is_empty(), "replica 2 retired although it was added back: {retired:?}");
    let ready = seen.iter().filter(|(i, a)| *i == 2 && matches!(a, Action::ReadReady { .. })).count();
    assert_eq!(ready, 0, "the retirement barrier is never reported to the application");
}

#[test]
fn a_joiner_removed_again_retires_after_its_epochs_were_pruned() {
    // Replica 3 joins (epoch 1) and learns that it leaves again (epoch 2), but is cut off
    // before the new members catch up; meanwhile a third change also without it takes
    // effect, and every peer prunes the epochs that knew it. It must still be able to catch
    // up, confirm and retire.
    let mut rs: Vec<Replica> = (0..4).map(|i| ordered(i, &[])).collect();
    let mut seen = run_without(&mut rs, &[]);
    let mut k = 0u32;
    let mut change = |rs: &mut Vec<Replica>,
                      seen: &mut Vec<(usize, Action)>,
                      members: &[u32],
                      order: &[u32],
                      cut: &[usize]| {
        let ids = |v: &[u32]| v.iter().copied().map(NodeId).collect::<Vec<_>>();
        rs[0].propose_reconfiguration(T0, ids(members), ids(order)).unwrap();
        // Everyone learns the change; `cut` is away for its window.
        seen.extend(run_without(rs, &[]));
        // Through the window.
        for _ in 0..70 {
            rs[0].propose(T0, Bytes::from(format!("w{k}"))).unwrap();
            seen.extend(run_without(rs, cut));
            k += 1;
        }
    };
    change(&mut rs, &mut seen, &[0, 1, 2, 3], &[], &[]);
    change(&mut rs, &mut seen, &[0, 1, 2], &[], &[3]);
    change(&mut rs, &mut seen, &[0, 1, 2], &[1, 0, 2], &[3]);
    assert!(seen.iter().any(|(i, a)| *i == 0 && matches!(a, Action::Reconfigured { epoch: 3, .. })));
    // Back: every request it sends comes from a replica no peer's schedule knows any more.
    for t in 1..40 {
        for r in &mut rs {
            r.handle_timeout(Timestamp(t * 1_000_000_000)).unwrap();
        }
        seen.extend(run_without(&mut rs, &[]));
    }
    assert!(
        seen.iter().any(|(i, a)| *i == 3 && matches!(a, Action::Retired { .. })),
        "replica 3 never retired (commit index {} of {})",
        rs[3].commit_index(),
        rs[0].commit_index()
    );
}

#[test]
fn a_replica_removed_while_down_learns_it_when_restarted() {
    // Replica 2 is down for its whole removal; the cluster then goes quiet. Nothing would
    // tell it what happened, so on restart it asks.
    let mut rs: Vec<Replica> = (0..4).map(|i| ordered(i, &[])).collect();
    let mut disks = vec![Vec::new(); 4];
    let run = |rs: &mut Vec<Replica>, disks: &mut Vec<Vec<Bytes>>, down: &[usize]| {
        let mut queue = std::collections::VecDeque::new();
        let mut seen = Vec::new();
        loop {
            for (i, r) in rs.iter_mut().enumerate() {
                for a in drain(r) {
                    match a {
                        Action::Send(o) => queue.push_back((o.to.0 as usize, Input::Message(o.bytes))),
                        Action::Persist { id, records, then, replace } => {
                            if replace {
                                disks[i].clear();
                            }
                            disks[i].extend(records);
                            queue
                                .extend(then.into_iter().map(|o| (o.to.0 as usize, Input::Message(o.bytes))));
                            queue.push_back((i, Input::Persisted(id)));
                        }
                        other => seen.push((i, other)),
                    }
                }
            }
            let Some((to, input)) = queue.pop_front() else { return seen };
            if down.contains(&to) {
                continue;
            }
            rs[to].handle(T0, input).unwrap();
        }
    };
    run(&mut rs, &mut disks, &[]);
    rs[0].propose(T0, Bytes::from_static(b"before")).unwrap();
    run(&mut rs, &mut disks, &[]);
    rs[0].propose_reconfiguration(T0, vec![NodeId(0), NodeId(1), NodeId(3)], vec![]).unwrap();
    run(&mut rs, &mut disks, &[2]);
    for k in 0..70u32 {
        rs[0].propose(T0, Bytes::from(format!("w{k}"))).unwrap();
        run(&mut rs, &mut disks, &[2]);
    }
    let behind = rs[2].commit_index();
    let mut cfg = config(&[0, 1, 2]);
    cfg.id = NodeId(2);
    cfg.seed = [9; 32];
    rs[2] = Replica::new(cfg, disks[2].clone(), T0).unwrap();
    // Its first requests are lost: it asks again until someone answers.
    let mut seen = run(&mut rs, &mut disks, &[0, 1, 3]);
    for t in 1..30 {
        seen.extend(run(&mut rs, &mut disks, &[]));
        for r in rs.iter_mut() {
            r.handle_timeout(Timestamp(t * 1_000_000_000)).unwrap();
        }
    }
    seen.extend(run(&mut rs, &mut disks, &[]));
    assert!(rs[2].commit_index() > behind, "replica 2 caught up after its restart");
    assert!(seen.iter().any(|(i, a)| *i == 2 && matches!(a, Action::Retired { epoch: 1 })));
}

#[test]
fn a_change_takes_effect_and_the_cluster_goes_quiet_without_writes() {
    // Nobody writes after the change: the window before it takes effect is filled anyway,
    // the removed replica retires, and then every replica is quiet.
    let mut rs: Vec<Replica> = (0..4).map(|i| ordered(i, &[])).collect();
    let mut seen = run_without(&mut rs, &[]);
    rs[0].propose_reconfiguration(T0, vec![NodeId(1), NodeId(2), NodeId(3)], vec![]).unwrap();
    seen.extend(run_without(&mut rs, &[]));
    let mut t = 1;
    while rs.iter().any(|r| r.next_deadline().is_some()) {
        assert!(t < 200, "still busy: {:?}", rs.iter().map(Replica::next_deadline).collect::<Vec<_>>());
        for r in &mut rs {
            r.handle_timeout(Timestamp(t * 100_000_000)).unwrap();
        }
        seen.extend(run_without(&mut rs, &[]));
        t += 1;
    }
    assert!(seen.iter().any(|(i, a)| *i == 3 && matches!(a, Action::Reconfigured { epoch: 1, .. })));
    assert!(seen.iter().any(|(i, a)| *i == 0 && matches!(a, Action::Retired { epoch: 1 })));
}

#[test]
fn a_change_takes_effect_without_writes_while_the_leader_is_down() {
    // The leader (0) is down when replica 1 proposes replacing it: the others fill the
    // window themselves, the change takes effect, and they go quiet.
    let mut rs: Vec<Replica> = (0..4).map(|i| ordered(i, &[])).collect();
    let mut seen = run_without(&mut rs, &[]);
    rs[1].propose_reconfiguration(T0, vec![NodeId(1), NodeId(2), NodeId(3)], vec![]).unwrap();
    seen.extend(run_without(&mut rs, &[0]));
    let mut t = 1;
    loop {
        for r in &mut rs[1..] {
            r.handle_timeout(Timestamp(t * 100_000_000)).unwrap();
        }
        seen.extend(run_without(&mut rs, &[0]));
        if rs[1..].iter().all(|r| r.next_deadline().is_none()) {
            break;
        }
        assert!(t < 400, "still busy: {:?}", rs.iter().map(Replica::commit_index).collect::<Vec<_>>());
        t += 1;
    }
    assert!(seen.iter().any(|(i, a)| *i == 3 && matches!(a, Action::Reconfigured { epoch: 1, .. })));
    let from = seen
        .iter()
        .find_map(|(_, a)| match a {
            Action::Reconfigured { epoch: 1, from, .. } => Some(from.0),
            _ => None,
        })
        .unwrap();
    assert!(rs[1..].iter().all(|r| r.commit_index() >= from), "the window was filled without the leader");
}

#[test]
fn a_restarted_single_member_goes_quiet() {
    // Nobody to ask where the log is: no catch-up timer either.
    let mut durable = Vec::new();
    let mut r = Replica::new(config(&[0]), [], T0).unwrap();
    settle(&mut r, &mut durable);
    let mut r = Replica::new(config(&[0]), durable.clone(), T0).unwrap();
    settle(&mut r, &mut durable);
    assert_eq!(r.next_deadline(), None);
}

#[test]
fn membership_changes_are_validated() {
    let mut r = ordered(0, &[]);
    drain(&mut r);
    for (members, order) in [(vec![], vec![]), (vec![1, 1], vec![]), (vec![1, 2], vec![2])] {
        let members = members.into_iter().map(NodeId).collect();
        let order = order.into_iter().map(NodeId).collect();
        assert!(matches!(r.propose_reconfiguration(T0, members, order), Err(Error::InvalidConfig(_))));
    }
}

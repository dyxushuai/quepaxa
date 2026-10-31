//! Contract tests of the public `Replica` API on a single-member cluster.

mod common;

use bytes::Bytes;
use common::{T0, config, drain, ordered, settle};
use quepaxa::{Action, Config, DropReason, Error, Input, NodeId, PersistId, Replica, Slot};

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
fn read_barrier_covers_earlier_commits_without_a_log_entry() {
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
    assert!(seen.iter().any(|a| matches!(a, Action::ReadReady { id, slot } if *id == read && *slot >= ws)));
    assert!(
        !seen.iter().any(|a| matches!(a, Action::Commit { slot, .. } if *slot > ws)),
        "a read takes no slot"
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

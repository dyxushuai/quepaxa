//! Contract tests of the public `Replica` API on a single-member cluster.

use bytes::Bytes;
use quepaxa::{
    Action, Config, DropReason, Error, Input, NodeId, PersistId, Replica, Slot, Timestamp,
};

const T0: Timestamp = Timestamp(0);

fn config(members: &[u32]) -> Config {
    Config::new(
        [9; 16],
        NodeId(members[0]),
        members.iter().copied().map(NodeId).collect(),
        [1; 32],
    )
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
                Action::Persist { id, records, .. } => {
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
        matches!(&actions[..], [Action::Persist { id: PersistId(0), records, then }] if records.len() == 1 && then.is_empty())
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
    assert!(
        matches!(&replay[0], Action::Commit { slot: Slot(0), entries } if entries[0].payload == "x")
    );
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
    assert_eq!(
        r.handle(T0, Input::Persisted(PersistId(5))),
        Err(Error::UnknownPersistId)
    );
}

#[test]
fn storage_failure_fences_and_drops_outstanding_proposals() {
    let mut r = Replica::new(config(&[0, 1, 2]), [], T0).unwrap();
    let mut disk = Vec::new();
    settle(&mut r, &mut disk);
    let id = r.propose(T0, Bytes::from_static(b"x")).unwrap();
    drain(&mut r);
    r.handle(T0, Input::StorageFailed(PersistId(1))).unwrap();
    assert_eq!(
        drain(&mut r),
        vec![Action::Dropped {
            id,
            reason: DropReason::Fenced
        }]
    );
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
    assert_eq!(
        r.propose(T0, Bytes::from(vec![0; 64])),
        Err(Error::PayloadTooLarge)
    );
    r.propose(T0, Bytes::new()).unwrap();
    drain(&mut r);
    assert_eq!(r.propose(T0, Bytes::new()), Err(Error::Busy));
}

#[test]
fn invalid_configs_are_rejected() {
    let mut dup = config(&[0, 0]);
    dup.id = NodeId(0);
    assert!(matches!(
        Replica::new(dup, [], T0),
        Err(Error::InvalidConfig(_))
    ));
    let mut stranger = config(&[0, 1]);
    stranger.id = NodeId(7);
    assert!(matches!(
        Replica::new(stranger, [], T0),
        Err(Error::InvalidConfig(_))
    ));
}

#[test]
fn garbage_network_input_is_counted_and_ignored() {
    let mut r = Replica::new(config(&[0, 1, 2]), [], T0).unwrap();
    drain(&mut r);
    r.handle(T0, Input::Message(Bytes::from_static(b"not a message")))
        .unwrap();
    assert!(drain(&mut r).is_empty());
    assert_eq!(r.metrics().messages_rejected, 1);
}

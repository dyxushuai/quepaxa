use super::*;
use crate::api::{Config, Input};
use crate::batch::{self, Entry, Item, Kind};
use crate::core_state::CoreState;
use crate::dedup::Dedup;
use crate::durable::Record;
use crate::proposal::{Proposal, ValueKey};
use crate::schedule::Schedule;
use alloc::vec;
use bytes::Bytes;

const NOW: Timestamp = Timestamp(0);

fn config() -> Config {
    Config::new([1; 16], NodeId(0), vec![NodeId(0)], [2; 32])
}

fn settle(r: &mut Replica, disk: &mut Vec<Bytes>) -> Vec<Action> {
    let mut seen = Vec::new();
    loop {
        let mut actions = Vec::new();
        r.poll_actions(&mut actions);
        if actions.is_empty() {
            return seen;
        }
        let mut last = None;
        for a in actions {
            match a {
                Action::Persist { id, records, replace, then } => {
                    assert!(then.is_empty(), "single-member cluster");
                    if replace {
                        disk.clear();
                    }
                    disk.extend(records);
                    last = Some(id);
                }
                other => seen.push(other),
            }
        }
        if let Some(id) = last {
            r.handle(NOW, Input::Persisted(id)).unwrap();
        }
    }
}

#[test]
fn request_id_ranges_exhaust_independently_without_reuse() {
    let mut r = Replica::new(config(), [], NOW).unwrap();
    let mut disk = Vec::new();
    settle(&mut r, &mut disk);
    r.next_proposal = READ_ID_BASE - 1;
    let write = r.propose(NOW, Bytes::new()).unwrap();
    assert_eq!(write, ProposalId(READ_ID_BASE - 1));
    assert!(settle(&mut r, &mut disk).contains(&Action::Committed { id: write, slot: Slot(0) }));
    let before = disk.clone();
    assert_eq!(r.propose(NOW, Bytes::new()), Err(Error::RequestIdExhausted));
    assert!(settle(&mut r, &mut disk).is_empty());
    assert_eq!(disk, before, "refusal must not emit durable state");
    // Exhausted writes do not prevent reads, including the last representable u64 id.
    r.reads.next_id = READ_ID_BASE - 1;
    let read = r.read_barrier(NOW).unwrap();
    assert_eq!(read, ProposalId(u64::MAX));
    assert!(settle(&mut r, &mut disk).contains(&Action::ReadReady { id: read, slot: Slot(0) }));
    assert_eq!(r.read_barrier(NOW), Err(Error::RequestIdExhausted));
    assert!(settle(&mut r, &mut disk).is_empty());
    assert_eq!(r.reads.len(), 0);
    let mut recovered = Replica::new(config(), disk.clone(), NOW).unwrap();
    settle(&mut recovered, &mut disk);
    assert_eq!(recovered.incarnation(), r.incarnation() + 1);
    assert_eq!(recovered.propose(NOW, Bytes::new()), Ok(ProposalId(0)));
    settle(&mut recovered, &mut disk);
    assert!(recovered.read_barrier(NOW).is_ok());
}

#[test]
fn exhausted_reads_do_not_prevent_logged_requests() {
    let mut r = Replica::new(config(), [], NOW).unwrap();
    let mut disk = Vec::new();
    settle(&mut r, &mut disk);
    r.reads.next_id = READ_ID_BASE;
    assert_eq!(r.read_barrier(NOW), Err(Error::RequestIdExhausted));
    assert!(settle(&mut r, &mut disk).is_empty());
    assert_eq!(r.propose(NOW, Bytes::new()), Ok(ProposalId(0)));
    settle(&mut r, &mut disk);
    assert_eq!(r.propose_reconfiguration(NOW, vec![NodeId(0)], Vec::new()), Ok(ProposalId(1)));
}

#[test]
fn legacy_snapshot_holes_and_high_write_ids_remain_valid_after_recovery_and_install() {
    // Older allocators interleaved read ids with writes and allowed writes in the upper half.
    let seen = [0, 2, READ_ID_BASE + 7];
    let mut dedup = Dedup::default();
    for id in seen {
        dedup.first_time(NodeId(0), 1, ProposalId(id));
    }
    let state = CoreState {
        next: 1,
        leaders: vec![NodeId(0)],
        schedule: Schedule::with_order(vec![NodeId(0)]),
        dedup,
        tuning: crate::schedule::TuningState::initial(128),
    }
    .encode();
    let mut disk = vec![
        Record::Incarnation(1).encode(),
        Record::Compacted { through: Slot(0), core_state: state.clone() }.encode(),
    ];
    let mut recovered = Replica::new(config(), disk.clone(), NOW).unwrap();
    settle(&mut recovered, &mut disk);
    let mut installed = Replica::new(config(), [], NOW).unwrap();
    settle(&mut installed, &mut Vec::new());
    assert!(installed.install_snapshot(NOW, Slot(0), state.clone()).unwrap());
    settle(&mut installed, &mut Vec::new());
    for r in [&recovered, &installed] {
        assert_eq!(r.core_state(), state, "historical ids must not be renumbered or discarded");
    }
    // Replaying an old duplicate must still skip it; a delayed id in a hole is still new.
    let entries: Vec<_> = seen
        .into_iter()
        .chain([1])
        .map(|id| Item {
            entry: Entry { origin: NodeId(0), incarnation: 1, id: ProposalId(id), payload: Bytes::new() },
            kind: Kind::App,
        })
        .collect();
    disk.push(
        Record::Decided {
            slot: Slot(1),
            proposal: Proposal {
                priority: 1,
                key: ValueKey { origin: NodeId(0), incarnation: 1, seq: 1 },
                value: batch::encode(&entries),
            },
        }
        .encode(),
    );
    let mut replayed = Replica::new(config(), disk.clone(), NOW).unwrap();
    let applied: Vec<_> = settle(&mut replayed, &mut disk)
        .into_iter()
        .filter_map(|a| match a {
            Action::Commit { entries, .. } => Some(entries),
            _ => None,
        })
        .flatten()
        .map(|e| e.id)
        .collect();
    assert_eq!(applied, [ProposalId(1)]);
    assert_eq!(replayed.propose(NOW, Bytes::new()), Ok(ProposalId(0)));
    settle(&mut replayed, &mut disk);
}

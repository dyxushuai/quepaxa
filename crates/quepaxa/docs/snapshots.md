# Snapshots and compaction

The log grows forever unless the application takes snapshots of its state. A snapshot
lets the replica discard every slot it covers, and lets a replica that fell far behind
start from the snapshot instead of the whole log.

The application owns snapshots: their format, where they are stored, how they travel.
The replica contributes a small opaque [`core_state`](crate::Replica::core_state) (the leader
schedule, membership and duplicate filter as of the snapshot) that must travel **with**
the application state.

## Taking a snapshot

1. After applying slot `through`, take your state and
   [`replica.core_state()`](crate::Replica::core_state) *at that moment* (before applying anything
   newer). [`emitted_index`](crate::Replica::emitted_index) is the number of slots applied so far,
   so `through = emitted_index() - 1`.
2. Store both **durably** (write, `fsync`, rename).
3. Call [`compact(now, Slot(through), core_state)`](crate::Replica::compact). The replica drops the
   covered slots from memory and emits a **checkpoint**: a [`Persist`](crate::Action::Persist)
   with `replace: true` whose records replace all earlier ones.

```
use bytes::Bytes;
use quepaxa::{Action, Config, Input, NodeId, Replica, Slot, Timestamp};

# fn settle(r: &mut Replica, disk: &mut Vec<Bytes>) -> Vec<Bytes> {
#     let mut committed = Vec::new();
#     let mut actions = Vec::new();
#     r.poll_actions(&mut actions);
#     while !actions.is_empty() {
#         for action in std::mem::take(&mut actions) {
#             match action {
#                 Action::Persist { id, records, replace, .. } => {
#                     if replace { disk.clear(); }
#                     disk.extend(records);
#                     r.handle(Timestamp(0), Input::Persisted(id)).unwrap();
#                 }
#                 Action::Commit { entries, .. } => committed.extend(entries.into_iter().map(|e| e.payload)),
#                 _ => {}
#             }
#         }
#         r.poll_actions(&mut actions);
#     }
#     committed
# }
let config = |seed| Config::new([7; 16], NodeId(0), vec![NodeId(0)], seed);
let (mut disk, mut state) = (Vec::new(), Vec::new());
let mut replica = Replica::new(config([1; 32]), [], Timestamp(0))?;
settle(&mut replica, &mut disk);
for payload in [&b"a"[..], b"b", b"c"] {
    replica.propose(Timestamp(0), Bytes::copy_from_slice(payload))?;
    state.extend(settle(&mut replica, &mut disk));
}

// 1. A consistent cut: state and core state after the same slot.
let through = replica.emitted_index() - 1;
let snapshot = (state.clone(), replica.core_state());
// 2. (store `snapshot` durably here)
// 3. Compact: the checkpoint replaces the durable log.
let records_before = disk.len();
replica.compact(Timestamp(0), Slot(through), snapshot.1.clone())?;
settle(&mut replica, &mut disk);
assert!(disk.len() < records_before);

replica.propose(Timestamp(0), Bytes::from_static(b"d"))?;
state.extend(settle(&mut replica, &mut disk));

// After a crash, the application's memory is gone. Restore its snapshot first, then start
// the replica from the records: it re-emits only the slots after the checkpoint, which
// apply on top of the snapshot.
let mut state = snapshot.0.clone();
let mut replica = Replica::new(config([2; 32]), disk.clone(), Timestamp(1))?;
state.extend(settle(&mut replica, &mut disk));
assert_eq!(state, [&b"a"[..], b"b", b"c", b"d"]);
// Then tell the replica about the snapshot: it may be newer than the last checkpoint that
// reached the disk (here it is not, so nothing changes).
let installed = replica.install_snapshot(Timestamp(1), Slot(through), snapshot.1)?;
assert!(!installed, "the checkpoint already covers it");
# Ok::<(), quepaxa::Error>(())
```

Compaction is cheap for the replica (it drops what the snapshot covers); the cost is the
application's snapshot itself. Take snapshots off the hot path: encode a consistent cut
quickly, write it on another thread, and call `compact` when the write is done (the
reference runtime does exactly this).

## Installing a peer's snapshot

When a replica needs slots its peers have already compacted away, it emits
[`SnapshotNeeded { peer, through }`](crate::Action::SnapshotNeeded). Fetch `peer`'s latest
snapshot (any transport: it can be large) and call
[`install_snapshot(now, Slot(snapshot_through), core_state)`](crate::Replica::install_snapshot):

- `Ok(false)`: this replica already applied everything the snapshot covers (it went stale
  in transit). Change nothing.
- `Ok(true)`: adopt the snapshot as your application state, then **store it durably
  before executing the checkpoint `Persist` the call queued** — that checkpoint drops the
  log below the snapshot, so a crash in between would leave neither. If you cannot store
  it, report [`StorageFailed`](crate::Input::StorageFailed) for that persist instead.
- [`Err(InvalidCoreState)`](crate::Error::InvalidCoreState): the core state does not belong with
  this snapshot or this cluster (corrupted, mismatched `through`, or built under another
  leader order).

`examples/restart.rs` shows a replica that was down while its peers compacted, coming back
and catching up through a peer's snapshot.

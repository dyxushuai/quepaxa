# Durability and restart

QuePaxa assumes crash-*recovery*: a replica may stop at any instant and come back with
whatever it had made durable. The crate defines exactly what that is — opaque records you
store and hand back — and is safe as long as your storage keeps four promises.

## What your storage must promise

1. **Order.** Records of one [`Persist`](crate::Action::Persist) are durable after those of
   every earlier persist. An append-only file written in action order, `fsync`ed before
   reporting [`Persisted`](crate::Input::Persisted), keeps this.
2. **Durability before acknowledgement.** Report `Persisted(id)` only when the records of
   `id` (and everything before) survive a power loss.
3. **Atomic replacement.** A persist with `replace: true` replaces *all* earlier records
   at once: after a crash you see either the old set or the new one. Write a new file,
   `fsync` it, rename it over the old one, `fsync` the directory.
4. **Torn tails are not records.** A crash in the middle of an append may leave a partial
   last record. Drop it (records are self-checking: a CRC-32C covers each), or pass it on
   and let [`Replica::new`](crate::Replica::new) report
   [`CorruptRecord`](crate::Error::CorruptRecord) — never hand back bytes that were not a
   complete record.

If storage fails, report [`Input::StorageFailed`](crate::Input::StorageFailed). The replica
**fences** itself: it stops sending and answering, reports every outstanding proposal as
[`Dropped`](crate::Action::Dropped) with [`DropReason::Fenced`](crate::DropReason::Fenced) (their
outcome is unknown — they may still commit, as after a crash), and returns
[`Error::Fenced`](crate::Error::Fenced) from then on. Restart the process to recover.

## Restarting

Pass every durable record, in write order, to [`Replica::new`](crate::Replica::new), with a
**fresh** random seed:

```
use bytes::Bytes;
use quepaxa::{Action, Config, Input, NodeId, Replica, Timestamp};

/// Runs a single-member replica until it is quiet; returns its payload commits.
fn settle(r: &mut Replica, disk: &mut Vec<Bytes>) -> Vec<Bytes> {
    let mut committed = Vec::new();
    let mut actions = Vec::new();
    r.poll_actions(&mut actions);
    while !actions.is_empty() {
        for action in std::mem::take(&mut actions) {
            match action {
                Action::Persist { id, records, replace, .. } => {
                    if replace { disk.clear(); }
                    disk.extend(records);
                    r.handle(Timestamp(0), Input::Persisted(id)).unwrap();
                }
                Action::Commit { entries, .. } => committed.extend(entries.into_iter().map(|e| e.payload)),
                _ => {}
            }
        }
        r.poll_actions(&mut actions);
    }
    committed
}

let config = |seed| Config::new([7; 16], NodeId(0), vec![NodeId(0)], seed);
let mut disk = Vec::new();

let mut replica = Replica::new(config([1; 32]), [], Timestamp(0))?;
settle(&mut replica, &mut disk);
replica.propose(Timestamp(0), Bytes::from_static(b"a"))?;
settle(&mut replica, &mut disk);
drop(replica); // crash: memory is gone, `disk` survives

// A fresh seed on every start: priorities must stay unpredictable.
let mut replica = Replica::new(config([2; 32]), disk.clone(), Timestamp(10))?;
// Recovered slots are emitted again, in order, for the application to rebuild its state.
assert_eq!(settle(&mut replica, &mut disk), [Bytes::from_static(b"a")]);
assert_eq!(replica.commit_index(), 1);
# Ok::<(), quepaxa::Error>(())
```

On every start the replica first persists a new **incarnation** number and sends nothing
until that is durable. Proposals are identified by `(origin, incarnation, id)`, so ids from
before a crash are never confused with new ones, and a leader never reuses its reserved
top priority for a slot it may have used it in before.

### What the application sees after a restart

- [`Commit`](crate::Action::Commit) is emitted again for slots the replica recovers from storage,
  and for slots it learns again from peers. Track the next slot you expect and skip older
  ones (or rebuild from your own snapshot, see [snapshots](crate::guide::snapshots)).
- Proposals made before the crash are not reported: their outcome is unknown. A client
  that retries must make the retry idempotent at the application level (for example with a
  request id in the payload). The log itself never commits one proposal twice.
- In a quiet cluster a restarted replica learns that it is behind from the next message it
  gets: the next write anywhere, or a read barrier or proposal of its own. It then catches
  up in bulk, from a peer's snapshot if the log was compacted.

## Things that break safety

- Acknowledging a write before it is durable, or reordering writes.
- Sending a `then` message before its persist is durable.
- Restarting a replica identity with an **empty** data directory (or an old backup) while
  the rest of the cluster remembers it: it would reuse its incarnation and leader
  reservation. Restore its latest state, or add it back as a new replica
  ([membership](crate::guide::membership)).
- Reusing the seed across starts (liveness, not safety: priorities become predictable).

Records are versioned: newer versions of the crate read older records, but older versions
may not read newer ones. The changelog lists format changes and upgrade notes.

# Linearizable reads

Every replica applies the same log, but a replica may be behind: reading its state
directly can return a value older than a write that already completed elsewhere. A
**read barrier** gives a read point that is linearizable — it reflects every write that
completed before the read started, on any replica.

[`Replica::read_barrier`](crate::Replica::read_barrier) asks recorders for their
*high-water mark*: the first slot past every slot they hold state for. A write completes
only once its slot is decided, and a slot is decided only after a majority of its members
recorded it durably, so the largest mark from a majority is past every completed write.
The replica waits until it has applied every slot below that mark (catching up, or driving
a slot nobody finishes), checks that a majority of every member set from there on answered
(two, while a membership change is pending), and emits
[`ReadReady { id, slot }`](crate::Action::ReadReady) with the last slot it applied.
`ReadReady` follows the `Commit` of `slot`, so with an in-order applier your state is
already there: read it and answer.

```
use bytes::Bytes;
use quepaxa::{Action, Config, Input, NodeId, Replica, Timestamp};

let cfg = Config::new([7; 16], NodeId(0), vec![NodeId(0)], [9; 32]);
let mut replica = Replica::new(cfg, [], Timestamp(0))?;
let mut state = std::collections::BTreeMap::new();
let mut actions = Vec::new();
let mut answer = None;

replica.poll_actions(&mut actions);
let mut pending: Vec<Action> = std::mem::take(&mut actions);
replica.propose(Timestamp(0), Bytes::from_static(b"x=1"))?;
replica.poll_actions(&mut actions);
pending.append(&mut actions);
let read = replica.read_barrier(Timestamp(0))?;
replica.poll_actions(&mut actions);
pending.append(&mut actions);

while !pending.is_empty() {
    for action in std::mem::take(&mut pending) {
        match action {
            Action::Persist { id, .. } => {
                replica.handle(Timestamp(0), Input::Persisted(id))?;
                replica.poll_actions(&mut actions);
                pending.append(&mut actions);
            }
            Action::Commit { entries, .. } => {
                for e in entries {
                    let (k, v) = e.payload.split_at(1);
                    state.insert(k.to_vec(), v[1..].to_vec());
                }
            }
            // Everything up to the barrier's slot is applied: the read is linearizable.
            Action::ReadReady { id, .. } if id == read => answer = state.get(&b"x"[..]).cloned(),
            _ => {}
        }
    }
}
assert_eq!(answer.as_deref(), Some(&b"1"[..]));
# Ok::<(), quepaxa::Error>(())
```

## Costs and alternatives

A read costs one round trip to a majority: no slot is decided and nothing is written.
Reads requested while a round is out share the next one, so many reads share each round.
While writes are in flight, the mark may point past slots still being decided, and the
read waits for them: about one commit latency, never longer than the slowest write it
must reflect. Reads that may be stale — monitoring, caches — can read local state directly.
The reference runtime offers both (`GET` and `STALE`).

A read also makes a lagging replica catch up: every answer carries the replier's decided
prefix, and the replica cannot report the read before it has applied everything the
majority has.

The rule is modelled in `verification/tla/Reads.tla` (checked with TLC across membership
changes) and proved for fixed membership in `ReadsProof.tla`. Reads count against
[`Config::max_outstanding_proposals`](crate::Config::max_outstanding_proposals) together
with proposals, and are reported [`Dropped`](crate::Action::Dropped) when the replica
fences.

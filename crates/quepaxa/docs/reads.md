# Linearizable reads

Every replica applies the same log, but a replica may be behind: reading its state
directly can return a value older than a write that already completed elsewhere. A
**read barrier** gives a read point that is linearizable — it reflects every write that
completed before the read started, on any replica.

[`Replica::read_barrier`](crate::Replica::read_barrier) proposes an empty marker through the
log. When it is decided at slot `s`, the replica emits
[`ReadReady { id, slot: s }`](crate::Action::ReadReady). Once the application has applied every
commit up to and including `s` — `ReadReady` is emitted in log order, after the `Commit`
of `s`, so with an in-order applier this is already true — read your state and answer.

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

A barrier costs one slot of consensus (it is batched with concurrent writes and barriers,
so under load many reads share one). Reads that may be stale — monitoring, caches — can
read local state directly. The reference runtime offers both (`GET` and `STALE`).

A barrier also makes a lagging replica catch up: it cannot report the barrier before it
has applied everything decided before it.

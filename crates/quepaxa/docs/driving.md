# Driving a replica

A [`Replica`](crate::Replica) is a state machine. It does no I/O and owns no threads: your
code delivers events to it and carries out the [`Action`](crate::Action)s it asks for. This
chapter is the contract between the two. Get it right once, in one place, and every
guarantee of the crate follows.

## The loop

Every interaction has the same shape:

1. **Call** exactly one of
   - [`Replica::propose`](crate::Replica::propose) / [`read_barrier`](crate::Replica::read_barrier) /
     [`propose_reconfiguration`](crate::Replica::propose_reconfiguration) — client requests;
   - [`Replica::handle`](crate::Replica::handle) with an [`Input`](crate::Input) — a message from a
     peer, or the completion (or failure) of a write;
   - [`Replica::handle_timeout`](crate::Replica::handle_timeout) — the clock reached
     [`next_deadline`](crate::Replica::next_deadline);
   - [`Replica::compact`](crate::Replica::compact) / [`install_snapshot`](crate::Replica::install_snapshot) — see
     [snapshots](crate::guide::snapshots).
2. **Drain** [`Replica::poll_actions`](crate::Replica::poll_actions) into a buffer. Until you do, the
   next call fails with [`Error::NotDrained`](crate::Error::NotDrained): this keeps an action from
   being forgotten between two calls.
3. **Execute** the actions in the order returned (below).
4. **Re-arm** one timer for [`next_deadline`](crate::Replica::next_deadline) (`None`: no timer is
   needed until the next call).

Every method takes the current time as a [`Timestamp`](crate::Timestamp): nanoseconds from a
monotonic clock whose epoch you choose. Time never moves backwards inside the replica, so a
slightly stale reading is harmless.

## The actions

| Action | What to do |
|---|---|
| [`Persist { id, records, then, replace }`](crate::Action::Persist) | Make `records` durable after every earlier persist (or, with `replace`, atomically replace all earlier records). **Then** send every `then` message, **then** call `handle(Input::Persisted(id))`. |
| [`Send(out)`](crate::Action::Send) | Send `out.bytes` to `out.to`. No durability needed. |
| [`Commit { slot, entries }`](crate::Action::Commit) | Apply `entries` to your state machine. Slots arrive in order, without gaps. |
| [`Committed { id, slot }`](crate::Action::Committed) | Your proposal `id` is in the log: answer the client. |
| [`ReadReady { id, slot }`](crate::Action::ReadReady) | Read barrier `id`: once `slot` is applied, serve the read. |
| [`SnapshotNeeded { peer, through }`](crate::Action::SnapshotNeeded) | Fetch `peer`'s application snapshot and install it. |
| [`Reconfigured { epoch, from, members }`](crate::Action::Reconfigured) | Membership changes at slot `from`. |
| [`Dropped { id, reason }`](crate::Action::Dropped) | Proposal `id` will not be reported again. |

`Action` is `#[non_exhaustive]`: keep a `_ => {}` arm so new actions in minor versions do
not break your build.

### Persist before send

The one rule that protects safety: **a message in `then` must not leave before `records`
are durable.** A recorder's reply promises that its state is on disk; if the reply left
first and the replica crashed, it could later contradict itself. The action type makes the
rule hard to break — the gated messages are *inside* the persist — and messages in
`Send` are never gated, so they can go out at once.

You may group writes (one `fsync` for several persists) and report only the last id:
[`Input::Persisted`](crate::Input::Persisted) acknowledges every persist up to and including the
id given. Ids increase strictly; reporting one that was never issued, or one already
acknowledged, is [`Error::UnknownPersistId`](crate::Error::UnknownPersistId). If a write fails,
report [`Input::StorageFailed`](crate::Input::StorageFailed): the replica fences itself (see
[durability](crate::guide::durability)).

Writes can run on another thread while the replica keeps handling messages: persists
issued meanwhile are simply acknowledged later. This is how the reference runtime gets
group commit.

## The network

[`Outbound`](crate::Outbound) messages are opaque bytes for one peer. Deliver what you receive,
unchanged, with `handle(Input::Message(bytes))`. The transport may lose, duplicate,
delay and reorder messages: the protocol retransmits what matters. Malformed messages, and
messages from other clusters or non-members, are counted in
[`Metrics::messages_rejected`](crate::Metrics::messages_rejected) and otherwise ignored — they
are never an error. Authenticate and encrypt the transport yourself if the network is not
trusted: the crate assumes crash faults, not malicious peers.

## Threads

`Replica` is `Send + Sync` but not internally synchronized: drive each replica from one
place (one thread or task, or behind a lock). The usual shape is one event loop per replica
that owns it, fed by channels from the network and the storage writer.

## A complete loop

A single-member cluster commits on its own, which makes the whole contract visible in a
few lines:

```
use bytes::Bytes;
use quepaxa::{Action, Config, Input, NodeId, Replica, Timestamp};
use std::collections::VecDeque;

let cfg = Config::new([7; 16], NodeId(0), vec![NodeId(0)], [42; 32]);
let mut replica = Replica::new(cfg, [], Timestamp(0))?;
let mut disk = Vec::new(); // stands in for an append-only, fsync'd file
let mut queue = VecDeque::new();
let mut actions = Vec::new();

// Startup: the replica first persists its new incarnation.
replica.poll_actions(&mut actions);
queue.extend(actions.drain(..));

let id = replica.propose(Timestamp(1), Bytes::from_static(b"set x 1"))?;
replica.poll_actions(&mut actions);
queue.extend(actions.drain(..));

let mut state = Vec::new();
while let Some(action) = queue.pop_front() {
    match action {
        Action::Persist { id, records, then, replace } => {
            if replace { disk.clear(); }
            disk.extend(records);              // 1. durable
            assert!(then.is_empty());          // 2. send `then` (nobody to send to here)
            replica.handle(Timestamp(2), Input::Persisted(id))?; // 3. report
            replica.poll_actions(&mut actions);
            queue.extend(actions.drain(..));
        }
        Action::Commit { entries, .. } => state.extend(entries.into_iter().map(|e| e.payload)),
        Action::Committed { id: done, .. } => assert_eq!(done, id),
        _ => {}
    }
}
assert_eq!(state, [Bytes::from_static(b"set x 1")]);
assert_eq!(replica.commit_index(), 1);
# Ok::<(), quepaxa::Error>(())
```

With several members the loop is the same, plus delivering messages between replicas and
firing timers. `examples/cluster.rs` in the repository runs three replicas in one process
on a driver written to be read (`examples/common/mod.rs`); `examples/kv-tokio` is a full
networked, fsync'd runtime.

## Timers

The replica asks for time only through [`next_deadline`](crate::Replica::next_deadline): request
retransmission, hedging (proposing an entry itself when the leader is slow), taking over a
slot whose proposer went quiet, and catch-up. Calling
[`handle_timeout`](crate::Replica::handle_timeout) early or late is safe: early does nothing that
is not due, late only delays recovery. None of these timers affects safety, only how fast
the cluster gets past a failure ([tuning](crate::guide::tuning)).

## Errors

Every fallible method returns [`Error`](crate::Error). Caller-contract errors
(`NotDrained`, `UnknownPersistId`, `InvalidCoreState`) mean a bug in the driver; `Busy`
and `ReconfigurationPending` are backpressure; `Fenced` means the replica stopped after a
storage failure and must be restarted. Errors never leave the replica in a broken state:
the call simply did nothing.

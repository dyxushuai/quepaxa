# Membership and leaders

## Members and majorities

A cluster of `n` members tolerates `⌊(n - 1) / 2⌋` crashed or partitioned members: three
tolerate one, five tolerate two. Every slot needs a majority of its members to make
progress; any member (in fact any replica) can drive any slot.

## The leader

One member at a time is the **leader**: the only one allowed the reserved top priority in
a slot's first step, which lets it decide in one round trip when a majority answers
(the fast path). Other replicas forward their clients' entries to it. The leader is an
optimization, not a requirement — if it is slow or gone, the others decide slots on their
own with randomized priorities, and after eight consecutive slots not led by the leader it
moves to the back of the order. Every replica computes the leader of every slot from the
decided log, so they agree without any election.

[`Config::leader_order`](crate::Config::leader_order) sets the preferred order (default: by
id). Put first the member with the fastest path to a majority: on a wide-area deployment
the leader's placement alone changes client latency about twofold. Every member must use the
same order; members configured differently ignore each other's messages rather than
disagree on the leader. The order is part of the cluster's identity for as long as its
state lives; change it with a membership change.

## Changing membership

[`Replica::propose_reconfiguration`](crate::Replica::propose_reconfiguration) proposes a new set of
members (and leader order) as an entry in the log. If it is decided in slot `s`, slots up
to `s + 63` keep the old members and slot `s + 64` onward use the new ones. The window is
the bound on how far ahead any replica drives slots, so every replica knows the members of
every slot it touches from the decided log alone — no joint quorums, no second protocol.
Each replica reports the change with [`Reconfigured { epoch, from, members }`](crate::Action::Reconfigured)
in log order. If nothing is written meanwhile, the leader fills slots `s + 1` to `s + 63`
with empty batches, so a change takes effect without traffic.

- **One change at a time.** Proposing while a change has not taken effect returns
  [`ReconfigurationPending`](crate::Error::ReconfigurationPending). Of two changes proposed
  concurrently on different replicas, the one decided first wins; every replica ignores the
  other (its proposer still sees `Committed`, but no `Reconfigured` follows).
- **Adding a replica.** Start it with the cluster's *original* [`Config`](crate::Config) — its
  own id need not be a member. Once the change is decided, members also send it their
  decisions, so it catches up (in bulk, or from a snapshot) during the window and votes
  from slot `s + 64`.
- **Removing a replica.** Keep it running until it reports
  [`Retired { epoch }`](crate::Action::Retired) (`QPX_ACTION_RETIRED` in C); then it may
  shut down. Applying the slots before `from` itself is not enough: those slots were
  decided by the old members, and if a majority of them leave before the new members know
  those slots durably, nobody can recover them and the cluster stalls. So a removed
  replica asks the new members for their progress, and once a majority of them durably
  know every slot before `from`, takes a read barrier of its own; it reports `Retired`
  when the barrier commits and no change decided before it has added the replica back.
  Only a change decided after that can still add it back: whoever proposes one keeps the
  replica running (or restarts it). A replica that was down for its whole removal learns
  of it when it restarts (it asks every known replica where the log is, until one
  answers); one that kept running but was cut off learns only when it hears from a
  member, so restart it.
- **Retiring a replica that cannot report it.** What `Retired { epoch }` waits for —
  a majority of that epoch's members durably holding the slots before it — is the same
  for every replica the change removed. So once any of them reports it, all replicas
  removed by that epoch may shut down, unless a later change added one back. This covers
  a removed replica that is down for good, or one whose known peers were all replaced
  while it was down (it cannot find the new members on its own).
- **Replacing the leader.** Put the new leader first in the new order.

```no_run
use quepaxa::{NodeId, Replica, Timestamp};
# fn get_replica() -> Replica { unimplemented!() }
let mut replica: Replica = get_replica();
// Replace member 0 with member 3, which becomes the preferred leader.
let members = vec![NodeId(1), NodeId(2), NodeId(3)];
let order = vec![NodeId(3), NodeId(1), NodeId(2)];
let id = replica.propose_reconfiguration(Timestamp(0), members, order)?;
// ... drain and execute actions as usual; watch for `Action::Reconfigured` everywhere,
// and on replica 0 for `Action::Retired { epoch: 1 }` before shutting it down.
# let _ = id;
# Ok::<(), quepaxa::Error>(())
```

`examples/membership.rs` runs this end to end: replica 3 joins, replica 0 leaves, and the
new majority commits on the fast path led by replica 3.

Over real processes, `examples/kv-tokio` in the repository does the same: joining
replicas get their address from `--peers`, the client command `RECONFIGURE <ids>
[LEADERS <ids>]` proposes a change, and a removed replica exits with status 0 when it
reports `Retired` (its `tests/membership.rs` replaces a member end to end).

**Upgrades:** a replica running a version without membership changes cannot decode a
change entry and would keep the old majorities. Propose changes only once every replica
runs 0.3 or later.

Not supported: witnesses (members that vote but keep no log) and automatic, latency-based
leader tuning.

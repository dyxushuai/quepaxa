# Configuration and tuning

[`Config::new`](crate::Config::new) gives defaults for a LAN cluster. Every field is public;
change what you need before [`Replica::new`](crate::Replica::new), which validates the result
and returns [`Error::InvalidConfig`](crate::Error::InvalidConfig) with the reason otherwise.

| Field | Default | What it controls |
|---|---|---|
| `cluster_id` | — | Messages carrying another id are ignored. Use a random value per cluster. |
| `id`, `members` | — | This replica and the initial members (at most 1024). `id` need not be a member for a joining replica. |
| `seed` | — | Randomness for priorities: fresh CSPRNG output on every start. |
| `leader_order` | by id | Preferred leaders ([membership](crate::guide::membership)). |
| `max_batch_bytes` | 1 MiB | Largest encoded batch (one slot). Must be the same on every member. |
| `max_message_bytes` | 16 MiB | Largest accepted message; at least `2 * max_batch_bytes + 1024`. |
| `max_in_flight_slots` | 8 | Slots one replica drives at once (pipelining), at most 64. |
| `max_outstanding_proposals` | 4096 | Local proposals not yet committed before [`Busy`](crate::Error::Busy). |
| `retransmit_interval` | 50 ms | When an unanswered request is sent again. |
| `hedge_min`, `hedge_max` | 1 ms, 1 s | Bounds of the adaptive hedge delay. |
| `silent_leader_takeover` | on | Stop waiting for a leader that has said nothing for about a commit latency since a forward. |
| `placement` | off | Move the leader to the member that gives writers the lowest commit latency. Enable it on every member or on none. |
| `placement_sustain`, `placement_hold` | 3 min, 10 min | How long a gain must last before a move, and how long any order change is left alone. |

## Throughput

Proposals are batched automatically: while a slot is in flight, new entries wait and go
into the next batch, so batches grow with load without any tuning. `max_in_flight_slots`
pipelines slots; more helps on high-latency links, at the cost of more contention when
replicas compete. `max_outstanding_proposals` bounds memory: beyond it `propose` returns
[`Busy`](crate::Error::Busy) and the client should back off rather than queue unboundedly.
The default holds a few hundred milliseconds of a busy server's load; a server that bounds
its own client queues can raise it to their size, so a brief CPU stall queues requests
instead of failing them.

On disk, latency is a few `fsync`s per commit and throughput depends on how many writes
each `fsync` carries: write on a dedicated thread and acknowledge in groups
([driving](crate::guide::driving)).

## Wide-area deployments

The defaults assume round trips of a few milliseconds.

- **`retransmit_interval`** must exceed the round trip to the farthest member of a
  majority, or every request is sent several times.
- **`hedge_max`** must exceed the slowest client commit path (forward to the leader, one
  round trip to a majority, decision back). Below it, followers stop waiting for a live
  leader and propose its entries themselves, which costs throughput.
- **`silent_leader_takeover`** (on by default): a follower that hears nothing at all from
  the leader for about a commit latency after forwarding hands its entries to the next
  member of the leader order and takes over the leader's unfinished slots, instead of
  waiting up to `hedge_max`. In the repository's five-region benchmark, recovery from a
  lost leader region drops from several seconds to under a second. A leader acknowledges
  forwards before any write (at most once per eighth of `retransmit_interval` per
  follower), so a slow disk does not make it look silent; silence shorter
  than twice that acknowledgement's round trip, than one round trip plus its cooldown, or
  than `hedge_min`, is never counted.
- **`leader_order`**: put the leader where a majority is closest.
- **`placement`** (off by default): while writes are being applied, each member probes
  every other member once a second and keeps a smoothed round trip to each; a round trip
  not sampled for four seconds counts as unknown. Probing stops four seconds after the
  last write, so an idle cluster stays quiet. Members with it off neither probe nor
  answer. The leader in force ranks every member by the commit latency it would give:
  each writer's round trip to it, weighted by how much that writer writes, plus its round
  trip to a majority for every write. When another member costs at most 80% of the
  leader at every round for `placement_sustain`, no order change happened within
  `placement_hold`, and at least about one write a second flows, the leader proposes an
  order with that member first (a normal leader-order change, in force 64 slots later).
  The gain must also exceed 5 ms a write, so round trips within one datacenter, which
  differ by a fifth on noise alone, never move the leader. A member any writer cannot
  reach is never placed first, and a leader that miss-based rotation demoted is not placed
  first again for a hold (twice as long each time it is demoted again soon).

  Round trips cannot see a leader slowed by its CPU or disk. So every member also records,
  each round, the writers' tail commit latency under the leader in force, and the members
  the round trips cannot tell apart (within a tenth, plus 5 ms a write) are measured: a
  leader whose own tenure is measured hands the lead to each such member not measured
  within eight holds, one sustained period each, then to the fastest if it is slower by
  three standard errors and by more than a tenth. Across regions, where the round trips
  already rank members apart, nothing is explored.

  Writes are attributed to the member they were proposed at, so clients should submit to
  their nearest replica; clients that send everything to the leader make it look best
  placed. A membership change without a `leader_order` puts the staying members in order
  of their cost (one rotation demoted recently last), new members last.

The repository's simulated five-region benchmark (`docs/benchmarks.md`) shows each effect.

## Timers never affect safety

Retransmission, hedging, slot takeover and catch-up only decide *when* a replica acts.
Any setting is safe; a bad one is slow.

## Observability

[`Replica::metrics`](crate::Replica::metrics) returns counters: rejected messages, retransmits,
catch-up retries, slots decided, fast-path decisions, forwarded and hedged entries, and
how much decided state is held in memory. Rising `hedged` with a healthy leader means
`hedge_max` is too low; rising `retransmits` means `retransmit_interval` is below the
round trip; `retained_slots` that keeps growing means the application is not snapshotting.

Where the leader is and why: the `epoch` and `leader_position` gauges give the epoch in
force and this replica's place in the leader order (0: it leads); `suspicions`,
`stepped_aside` and `placement_moves` count why the leader moved. The share of slots
decided on the fast path is `fast_path_decisions` over `slots_decided`.
[`Replica::members`](crate::Replica::members) lists the members in force in leader order,
with each one's measured round trip (with `placement`), the commit index its latest
message carried, and when it was last heard from. In C the scalars are `QPX_METRIC_*`
values of `qpx_metric`; the per-member view is Rust only.

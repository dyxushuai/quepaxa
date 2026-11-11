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
| `tuning` | on | Try every member as leader, then lead from where writers commit fastest. Enable it on every member or on none. |
| `hedge_tuning` | on | With `tuning`, try hedge multipliers and keep the fastest. |
| `tuning_epoch_slots` | 128 | Slots per tuning epoch, at least 65. |
| `tuning_min_samples` | 32 | Fewest writers' entries for an epoch's latency to count. |
| `tuning_reexplore` | 30 min | How often the cluster explores again while writes flow (0: only when the leader falls behind). |

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
- **`tuning`** (on by default), the paper's leader tuning (§5.3): slots are divided into
  epochs of `tuning_epoch_slots`. An exploration gives every member, in id order, two
  epochs as leader, then one more epoch under the last one while the measurements arrive;
  each member reports, at every epoch's end, the commit latency of its own clients'
  writes by the leader of their slot. The order then becomes the members sorted by the
  latency writers saw under each (writers weighted by how much they write), best first.
  The cluster explores again when the leader's latest epoch is slower than the next
  member's measurement by more than three standard errors, or every `tuning_reexplore`.
  Each change is an ordinary leader-order change, in force 64 slots after its decision,
  so on a WAN an exploration of five members takes about a minute, during which far
  members lead for a few seconds each. Latency is measured where writes are proposed:
  clients should submit to their nearest replica. A member whose epochs had fewer than
  `tuning_min_samples` entries (it was down, or rotated out) is ranked last. An
  operator's leader-order or membership change ends an exploration and is kept until
  the next one. Nothing happens without writes: an idle cluster sends no tuning
  messages. Members with it off neither report nor propose; when one leads, the next
  member proposes in its place. A membership change without a `leader_order` keeps the
  order of the members that stay, new members last.
- **`hedge_tuning`** (on by default, with `tuning`): once the order is exploited, each
  replica tries hedge multipliers 1, 2, 4 and 8 of its commit latency for two epochs
  each and keeps 4 unless another is faster by more than three standard errors. The
  multiplier sets when the replica hedges its own entries and takes over slots others
  started; silence detection and stepping aside keep four times the commit latency.

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
`stepped_aside` and `tuning_moves` count why the leader moved, `tuning_phase` says whether
tuning is exploring (1) or exploiting (2), and `hedge_multiplier` gives this replica's. The share of slots
decided on the fast path is `fast_path_decisions` over `slots_decided`.
[`Replica::members`](crate::Replica::members) lists the members in force in leader order,
with the latency writers saw while it led (with `tuning`), the commit index its latest
message carried, and when it was last heard from. In C the scalars are `QPX_METRIC_*`
values of `qpx_metric`; the per-member view is Rust only.

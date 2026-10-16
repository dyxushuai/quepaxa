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

## Throughput

Proposals are batched automatically: while a slot is in flight, new entries wait and go
into the next batch, so batches grow with load without any tuning. `max_in_flight_slots`
pipelines slots; more helps on high-latency links, at the cost of more contention when
replicas compete. `max_outstanding_proposals` bounds memory: beyond it `propose` returns
[`Busy`](crate::Error::Busy) and the client should back off rather than queue unboundedly.

On disk, latency is a few `fsync`s per commit and throughput depends on how many writes
each `fsync` carries: write on a dedicated thread and acknowledge in groups
([driving](crate::guide::driving)).

## Wide-area deployments

The defaults assume round trips of a few milliseconds.

- **`retransmit_interval`** must exceed the round trip to the farthest member of a
  majority, or every request is sent several times.
- **`hedge_max`** must exceed the slowest client commit path (forward to the leader, one
  round trip to a majority, decision back). Below it, followers stop waiting for the
  leader and propose themselves, which costs throughput.
- **`leader_order`**: put the leader where a majority is closest.

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

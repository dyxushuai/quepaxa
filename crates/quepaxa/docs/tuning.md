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
| `max_in_flight_slots` | 8 | Maximum slots one replica drives at once (pipelining), at most 64; active concurrency can be adjusted within `1..=max_in_flight_slots` via [`Replica::set_in_flight_slots`](crate::Replica::set_in_flight_slots). |
| `tune_in_flight_slots` | off | Adapt active `in_flight_slots` online within `1..=max_in_flight_slots` while leading, using batch saturation, smoothed bytes-per-RTT, leader slot residence, and live follower commit lag (`#175`). |
| `max_outstanding_proposals` | 4096 | Local proposals not yet committed before [`Busy`](crate::Error::Busy). |
| `retransmit_interval` | 50 ms | The shortest wait before an unanswered request is sent again; resends back off from there. |
| `hedge_min`, `hedge_max` | 20 ms, 1 s | Bounds of the adaptive hedge delay. |
| `silent_leader_takeover` | on | Stop waiting for a leader that has said nothing for about a commit latency since a forward. |
| `placement` | off | Move the leader to the member that gives writers the lowest commit latency. Enable it on every member or on none. |
| `placement_sustain`, `placement_hold` | 3 min, 10 min | How long a gain must last before a move, and how long any order change is left alone. |
| `inline_batch_bytes` | `1024` | Largest forwarded chunk (bytes, at least 64) sent inline; larger chunks travel directly to every member as data batches and by reference in consensus. Set to `u32::MAX` to keep every chunk inline. Per replica. |
| `content_cache_bytes` | `None` (`max_batch_bytes × max_in_flight_slots`) | Per-origin byte budget for unpinned data batches in the content store (at least `max_batch_bytes`). Set explicitly to size the store independently of `max_in_flight_slots`; lowering active concurrency at runtime never shrinks it. |

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

### Online `in_flight_slots` tuning (`tune_in_flight_slots`)

With `tune_in_flight_slots` enabled (off by default; `--tune-in-flight-slots on` in
`kv-tokio`), the leader adapts its active concurrency over a power-of-two ladder within
`1..=max_in_flight_slots` without changing safety or memory budgets (`content_cache_bytes`
stays sized by `max_in_flight_slots`). It starts at `1`, steps up one rung per 50 ms on
batch saturation, holds a smoothed bytes-per-RTT demand floor, and explores adjacent rungs
only when leader p90 residence improves by at least `max(8%, 10 ms)` without retransmits or
follower commit-lag regression. When load drops or distress persists above the demand floor,
it steps `W` back down; losing leadership resets it. See `docs/benchmarks.md` ("Online
in-flight slots tuning (#175)") for LAN and WAN measurements.

### Batch references (`inline_batch_bytes`)

By default (`1024` bytes), a forwarded chunk of application entries larger than 1 KiB is
broadcast directly from its origin to every member as `Data` and named by a 33-byte
reference in `Forward` and the slot value, evening egress across replicas and avoiding
writing large payloads twice on disk. Smaller chunks (such as 17-byte commands at moderate
rates) stay inline automatically. In the repository's 1 Gbit/s LAN and five-region WAN
A/B benchmarks (`docs/benchmarks.md`, "#172 Phase 3"), `1024` raised 4 KiB and 64 KiB
saturation throughput 1.6–2.3× over inline Phase 1 (3.3–4.0× over Phase 0), cut 4 KiB WAN
p99 latency 2.1–4.9× (eliminating collapse under jitter/loss), and left 17-byte workloads
within noise with zero fetches on the WAN.

Raise `inline_batch_bytes` (or set it to `u32::MAX` to disable references) if your
workload has clients writing only to the leader (nothing is forwarded), or on a quiet WAN
where non-triangle-inequality routes make direct `Data` broadcasts routinely lose the race
to the leader's `RecordRequest` and drive up `fetches` and `parked_requests`. Because the
setting is per replica, mixed values across a cluster are safe.

## Wide-area deployments

The defaults assume round trips of a few milliseconds.

- **`retransmit_interval`** is a floor, not a period: an unanswered record request is
  first sent again after it or after twice the smoothed first-step round trip, whichever
  is longer, and each further resend of the same step waits twice as long, up to
  `hedge_max` (or the floor, if that is longer). Set it above the round trip to the farthest member of a majority, or the
  first requests are sent twice before a round trip has been measured. Lowering it does
  not resend sooner once the round trip is known.
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
catch-up retries, slots decided, fast-path decisions, forwarded and hedged entries (with
`hedges_at_bound`, those hedged only by the four-`hedge_max` bound although the leader kept
deciding, and `forward_resends` and `forward_resent_bytes`, forwards the leader had not
acknowledged in time), batch dissemination and fetch activity (`data_sent`, `fetches`,
`fetch_retries`, `fetches_served`, `parked_requests`), and
how much decided state is held in memory. Rising `hedged` with a healthy leader means
`hedge_max` is too low; rising `retransmits` means messages are lost or the link stays
saturated past even the backed-off resend waits; rising `fetches` or `parked_requests` on a
quiet network means direct `Data` broadcasts are losing the race to `RecordRequest`s (raise
`inline_batch_bytes` on a WAN); `retained_slots` that keeps growing means the application is
not snapshotting.

Where the leader is and why: the `epoch` and `leader_position` gauges give the epoch in
force and this replica's place in the leader order (0: it leads); `suspicions`,
`stepped_aside` and `placement_moves` count why the leader moved. Active pipeline concurrency
is reported in `in_flight_slots`, and runtime changes (from `tune_in_flight_slots` or
[`Replica::set_in_flight_slots`](crate::Replica::set_in_flight_slots)) in
`pipeline_adjustments`. The share of slots
decided on the fast path is `fast_path_decisions` over `slots_decided`.
[`Replica::members`](crate::Replica::members) lists the members in force in leader order,
with each one's measured round trip (with `placement`), the commit index its latest
message carried, and when it was last heard from. In C the scalars are `QPX_METRIC_*`
values of `qpx_metric`; the per-member view is Rust only.

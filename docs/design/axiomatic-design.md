# Design matrix

Suh's independence axiom, applied to the engine: each functional requirement (FR) should be
set by its own design parameter (DP). In the matrix {FR} = [A]{DP}, a diagonal is
uncoupled, a triangle (after reordering rows and columns) is decoupled and must be set in
that order, anything else is coupled: changing one DP moves several FRs that feed back on
each other. Loops whose time scales differ by orders of magnitude (hedging in
milliseconds, placement in minutes) count as decoupled in practice.

## Matrix

`X` marks a DP that affects an FR. Columns are in the order the DPs are set.

| | DP1 persist-before-send gate | DP2 ISR proposers/recorders | DP3 schedule window (LAG) | DP4 dedup | DP5 batch IDs | DP6 hedge delay δ | DP7 rotation, step-aside | DP8 placement | DP9 in-flight slots W | DP10 observer follow interval |
|---|---|---|---|---|---|---|---|---|---|---|
| FR1 acknowledged data survives crashes | X | | | | | | | | | |
| FR2 agreement | X | X | X | | X | | | | | |
| FR3 safe membership changes | | X | X | | | | | | | |
| FR4 at-most-once | | | | X | | | | | | |
| FR5 liveness without timeouts | | X | | | X | X | X | | | |
| FR6 low latency | | | | | X | X | X | X | X | X |
| FR7 bounded memory | | | | | X | | | | | |

**Safety (FR1–FR4) is decoupled.** It depends on DP1–DP5 only, in a triangle, and on no
timing parameter: timers decide when a replica acts, never whether the result is correct.
TLAPS, Verus and the model checker hold this; keep it that way.

**Liveness and performance (FR5–FR7) are coupled.** Known loops:

| Loop | Mechanism | Status |
|---|---|---|
| δ ↔ commit latency | δ is four times the smoothed commit latency; hedging contention stretches commit latency | cut for hedged commits (#121) and for own commits emitted while the local disk is stalled (`Health::Stuck` / `SharedStall`, preventing stall samples from stretching `4δ` past `aside_at`); #252 eliminated the WAN cold-start/congestion trigger by coalescing follower forwards, adding a fetch arrival grace with retry backoff, and separating `OutboundClass` (`Control` / `Consensus` / `Bulk`) streams |
| hedging ↔ rotation | a successful hedge decides a slot with a follower's value, which counts toward rotating the leader; rotation changes whom hedging targets | cut (#258): batches proposed while hedging around a talking leader carry a hedge flag and are neutral to `Schedule::observe` |
| W ↔ admission, content store | both budgets were derived from W, so W could not move without them | decoupled (#251): content store (`content_cache_bytes`, defaulting to `2 × max_batch_bytes × max_in_flight_slots` and sized to `≥ 2 × admission_bytes` in `kv-tokio`) and `kv-tokio` admission (`16 × max_batch_bytes`, `--admission-bytes`) stay fixed when active `in_flight_slots` moves so origins never self-evict unpinned `Data` batches at peers |
| W ↔ commit latency, follower lag | online `in_flight_slots` tuner (`tune_in_flight_slots`, DP9) reads saturation, smoothed bytes-per-RTT, leader slot residence, and follower `commit_index` lag to adjust active `W` within `1..=max_in_flight_slots` | bounded (#175): FR1–FR4 never read `W`, FR7 stays fixed at `max_in_flight_slots` (#251); deputy and stalled-slot takeover stay at the fixed ceiling (`min(max_in_flight_slots, 8)` / `max_in_flight_slots`), leader slot pacing (`request_rtt / W`) prevents convoying during upward probes, `w_floor` uses propagation `min_rtt` so congestion-inflated RTT cannot lock `W` high, saturation jumps are 50 ms cooldown-limited and suppressed during distress, upward probes require ≥max(8%, 10 ms) gain and abort on distress or follower-lag regression, sustained distress steps `W` down directly, and leadership changes reset the tuner |
| rotation ↔ placement | rotation's demotions feed placement; placement's orders can be undone by rotation | bounded by exclusions and the Verus placement proof (#199) |
| eviction ↔ fetch grace ↔ commit latency | unpinned `Data` batches trimmed under slow-disk burst backlogs triggered `fetch_grace` (`2 × commit_latency`) on later `RecordRequest`s, inflating `commit_latency`, retransmits, and pinning `W = 1` | cut: `content::Store::was_evicted` tracks the highest trimmed `(incarnation, seq)` per origin so already-evicted batches skip `fetch_grace` and fetch immediately while fresh in-flight `Data` still honors arrival grace |

## Rule for changes

A PR that adds or changes a mechanism says which FRs its DP affects. It may add an `X`
below or left of the diagonal (a DP set later reading one set earlier); one that adds an
`X` above it, or makes a DP read its own effect, introduces a loop and needs a reason here.

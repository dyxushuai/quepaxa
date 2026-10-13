# How are the hedging delay and leader order chosen

Parent: [map](../map.md)
Type: grilling
Status: resolved
Blocked by: 06

## Question

The paper gives no δ-tuning algorithm and the Go impl uses a static δ plus an epoch-based leader-order tuner agreed through a log pseudo-command. Decide: the policy trait boundary in the core, the default leader-order tuner (and whether its agreement rides the log), how δ is set or adapted, what counts as evidence that suppresses a hedged proposer (per step vs per slot), and how each is tested deterministically.

## Answer

Resolved 2026-10-13 under the "一步到位" directive.

1. **Leader per slot (closes A8)**: the leader schedule is a core-internal log entry `ScheduleChange { schedule }`. The schedule in force for slot k is the latest one decided at a slot ≤ k − W (W = `max_in_flight_slots`); a proposer may start slot k only once slot k − W is decided, so every proposer agrees on k's leader before k starts. Bootstrap schedule = members sorted by `NodeId`.
2. **Default leader tuner (closes A3, A4, A5-partially)**: the paper's epoch tuner, corrected: epochs of `epoch_slots` (default 256); explore = 2 epochs per member round-robin (2n epochs); exploit = schedule sorted **ascending** by mean epoch commit latency (fastest first), re-evaluated each epoch, re-explore only if the leader is slower than the next in schedule. Only the current leader proposes `ScheduleChange`. Behind a crate-internal `LeaderPolicy` trait, deterministic from inputs; static schedule available via config.
3. **Hedging delay δ**: `HedgePolicy` with default adaptive δ = clamp(2 × EWMA of fast-path commit latency, `hedge_min`, `hedge_max`) (defaults 1 ms / 5 s); static δ via config. Proposer at schedule position k waits `(k−1)·δ` — no pipeline-depth multiplier (Go's deviation not adopted).
4. **Hedge evidence (closes A7)**: per slot, per step: a hedged proposer's timer restarts whenever its colocated recorder observes progress (step advance) on that slot or the slot is decided; it activates only after `(k−1)·δ` without progress.
5. **Non-leader submissions**: a non-leader forwards its local proposals to the current leader with `Forward` (not durable) and starts its own hedged proposal for them if they are not committed within its hedge delay. **Commit-time dedup** (closes A9 for local proposals): the core skips any committed entry whose `(origin, incarnation, proposal_id)` already committed, using per-origin watermark + bitmap of width `max_outstanding_proposals`; this dedup state is part of the core snapshot state. Amends the core API guarantee to "a local `ProposalId` appears in at most one `Commit` entry".
6. **Tests**: deterministic sim scenarios from the Go grid — leader crash mid-run, a slow-proposer attack (delayed egress for a minority), tuner convergence to the fastest member, δ adaptation after latency shifts; liveness asserted as "commits resume within bound after heal".

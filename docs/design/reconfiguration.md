# Membership reconfiguration

Status: **implemented** (option A below), in the Rust API and the C ABI (0.5).
Tracking: #51. Scope: changing the set of replicas (and the leader order) of a running
cluster without stopping it; dynamic join of a new replica; removal. Witnesses (vote-only
members) are discussed but deferred.

## What the current design already gives us

- Every message carries `cluster_id` and `config_epoch` (always 0 today); receivers drop
  messages from another epoch, so epochs need no wire break.
- A replica drives only slots below `committed + max_in_flight_slots`, and
  `max_in_flight_slots <= 64 = LAG`. So once slot `s` is decided at some replica, no correct
  replica can have *started* a slot `>= s + 64` without first learning every slot `<= s`
  (it would need `committed > s` to start it).
- The leader schedule already works this way: a rotation decided in slot `s` takes effect at
  `s + LAG`, so every replica switches at the same slot without extra agreement.
- Durable state is rebuilt from the log plus `core_state`; `core_state` already records
  configuration-derived state (the leader order, since version 3).
- QuePaxa's safety is per slot: a slot's recorders are a fixed set, and the G1-G5 lemmas
  need every proposer of that slot to use the same set and the same majority size.

## Options

### A. Epoch change through the log, effective a window later (recommended)

A reconfiguration is a control entry in a batch (like a read barrier), carrying the new
epoch number, members and leader order. If it is decided in slot `s`, slots `>= s + 64`
belong to the new epoch: their recorders are the new members, quorums are majorities of
the new members, and their messages carry the new `config_epoch`. Slots `< s + 64` finish
under the old epoch.

This is Lamport's α-window ("Reconfiguring a State Machine", α = 64 here) and how the
schedule already changes. It reuses the in-flight bound as the safety argument: the
configuration of every slot is a function of the decided prefix before it, so all
proposers of a slot agree on its recorders.

- One change at a time: a second reconfiguration is rejected until the first takes effect.
- A joining replica starts as a non-voting learner: it catches up (bulk catch-up or a
  snapshot) before its first epoch slot; it can be proposed only after it has caught up to
  within a window, so the new epoch does not start with a majority still catching up.
- A removed replica keeps serving old-epoch slots until `s + 63` is decided, then stops
  (it still answers catch-up so laggards can finish).
- Cost: up to 64 slots of latency between deciding a change and its effect; fine for
  membership, which changes rarely.

### B. Joint consensus (Raft-style)

Slots between the two changes need majorities of *both* configurations. It lets the new
configuration take effect immediately, but every proposer and recorder has to handle dual
quorums, the G-lemmas would have to be re-proved for dual quorums, and the model checker
would need a second kind of quorum. More code in the safety-critical path for a benefit
(no α delay) that membership changes rarely need.

### C. Single-server changes only

Adding or removing one replica at a time makes any two consecutive majorities intersect,
so no α window is needed in Raft. In QuePaxa a slot can be in progress under both
configurations at once (randomized rounds, hedging), so overlapping majorities do not by
themselves keep G1; it would still need A's per-slot epoch rule, and then the
restriction buys nothing.

## As built (A)

- **Membership is the leader order.** The schedule already records, per slot range, an
  order of replicas; an order is the epoch's members, preferred leaders first. A change
  decided in slot `s` appends `(s + 64, new order)` and resets rotation counting, so the
  members of a slot (its recorders and majority) come from the same structure, and the same
  window bound, as its leader. Rotations within an epoch move down its own order.
- **API**: `Replica::propose_reconfiguration(now, members, leader_order) -> Result<ProposalId>`
  (empty order = by id); the id is reported like a proposal's. `Action::Reconfigured
  { epoch, from, members }` in log order when the change is applied (effective at `from`).
  `Error::ReconfigurationPending` while a change has not taken effect; a concurrent change
  decided while another is pending is ignored by every replica, and so is a duplicate of
  an applied change (hedging can decide an entry twice), so a late copy never reverts.
- **Entries**: a change is a batch entry of a third kind next to application data and read
  barriers; it is never handed to the application in `Commit`.
- **Joining**: a new replica runs with the original `Config` (its id need not be a
  member). Once the change is decided, members also send it their decisions, so it catches
  up (bulk catch-up or a snapshot) during the window.
- **Leaving**: a removed replica keeps serving until it has applied the slot before
  `from`; then the operator may stop it. Its messages are still accepted while it appears
  in the schedule.
- **State**: `core_state` version 4 carries the epoch, its first slot, and the applied
  change ids with the schedule; versions 2 and 3 are still read.
- **Mixed versions**: a replica that predates membership changes cannot decode the change
  entry and would keep the old majorities, so changes must only be proposed once every
  replica runs a version that has them (0.3+). The library cannot check this itself.
- **Recorders** need no epoch check: a proposer counts only replies from its slot's
  members, and registers are per slot.

## Verification

- Unit tests: effect a window later, one change at a time, late duplicates ignored, a change
  superseding a rotation counted in the same slot, epochs round-tripping through
  `core_state`, a member replaced end to end (the joiner catches up, the new majority
  commits without the removed replica).
- Simulator: `membership_changes_under_faults_stay_safe_and_converge` adds random changes
  over members and two spare replicas to the hostile profile (loss, duplication,
  partitions, crashes, snapshots); removed replicas are shut down once their last epoch is
  applied and restarted if added back. G1/G3/G4/G5 and linearizability are checked as
  everywhere else (1,000 seeds pass).
- Canaries: a change taking effect early (caught by G5), a duplicate applied twice (unit
  test), proposers using the configured members instead of the slot's (the cluster stalls).
- Not done: model checking (a change needs a 64-slot window, beyond the checker's budget).

## Decisions taken (defaults, revisitable)

1. Option A, one change at a time.
2. Witnesses later: they change recovery (a witness cannot serve catch-up).
3. A change carries the new leader order, so the order can change with membership (the
   configured `leader_order` still cannot change for existing state).
4. Rust first; the C ABI followed (0.5).

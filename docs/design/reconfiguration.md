# Membership reconfiguration (design, proposed)

Status: **proposed** — needs the decisions under "Open questions" before implementation.
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

## Recommended design (A) in more detail

- **API**: `Replica::propose_reconfiguration(now, members, leader_order) -> Result<ProposalId>`;
  `Action::Reconfigured { epoch, members, from_slot }` when it takes effect; `Config` stays
  the *initial* configuration. Errors: `ReconfigurationPending`, `InvalidConfig`.
- **State**: per-epoch configuration `(epoch, first_slot, members, leader_order)` kept in
  memory, persisted as a durable record when decided, and carried in `core_state` (new
  version) so a snapshot covers it. A replica restarted from an old snapshot learns later
  epochs from the log.
- **Recorders**: a register is created only by a member of the slot's epoch; requests for
  a slot from a non-member of its epoch are rejected.
- **Proposers**: quorum size and leader schedule come from the slot's epoch; the schedule
  restarts from the new leader order at `from_slot`.
- **Wire**: `config_epoch` in the envelope is the epoch of the slot the message is about
  (catch-up messages carry the sender's latest epoch); receivers accept any epoch they know
  and buffer or drop messages for epochs not yet known.
- **Dedup and read barriers**: unchanged (they are per origin and per slot).

## Verification plan

- Invariants: G1-G5 per slot with the slot's own recorder set; a new invariant that every
  replica assigns the same epoch to every slot it touches.
- Simulator: random reconfigurations (add, remove, replace the leader's replica) under the
  hostile profile, crash-restart and snapshots; linearizability unchanged.
- Model checker: a three-to-four-to-three replica scenario with one reconfiguration within
  the deviation budget.
- Canaries: reconfiguration taking effect before the window; a removed member still
  voting in a new-epoch slot; a joining member voting before it caught up.

## Open questions (need a decision)

1. **Option A** (window, one change at a time) — or is immediate effect worth joint
   consensus?
2. **Witnesses**: in scope for the first version (members that vote but hold no log), or
   later? They change recovery (a witness cannot serve catch-up).
3. **Leader order changes**: allow them through the same reconfiguration entry (this also
   lifts today's "fixed for the life of the cluster" rule for `leader_order`)?
4. **C ABI**: expose reconfiguration in the same release, or Rust first?

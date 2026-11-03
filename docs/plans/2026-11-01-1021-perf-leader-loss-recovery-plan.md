---
title: Faster Recovery From a Lost Leader Region - Plan
type: perf
date: 2026-11-01
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-plan-bootstrap
execution: code
---

# Faster Recovery From a Lost Leader Region - Plan

## Goal Capsule

- **Objective:** When the leader's region crashes or is cut off in a cross-region cluster, writes in the other regions resume within about two commit latencies instead of after about 3.2 s, and the backlog drains within a few seconds. Clients see no errors, healthy-leader latency does not regress, and the flat-network fault results do not regress.
- **Means:** followers that stop hearing from the leader suspect it, stop forwarding to it, propose directly, and take over its unfinished slots sooner (KTD1-KTD3). Detection is early only when the leader is silent (KTD3). The leader schedule, rotation rule and every safety argument stay as they are (KTD5).
- **Authority:** Requirements win on behavior; KTDs win on mechanism; units implement both and override neither.
- **Stop conditions:**
  - Stop and report if U1's measurement does not support the diagnosis below. The design rests on it.
  - Stop and report if early detection cannot avoid hedging on a healthy leader in `run.sh`'s slow-disk or far-leader scenarios, or in `wan.sh`'s slow and jitter cases. A change that trades the 3 s stall for routine hedge storms is not done.
  - Stop and report if any change would need the schedule's rotation or window rule to change (KTD5).
- **Execution profile:**
  1. Measure in the simulator first (U1).
  2. Then suspect mode (U2) and silence detection (U3), test-first.
  3. Then verification (U4) and before/after benchmarks with docs (U5).
- **Ships via:** pull requests to `main` under the repository's usual review (CI, Codex review, cross-model review for U2-U3).

---

## Product Contract

### Summary

Two follower-side changes in the replica's proposer.

- **Suspect mode.** Once a follower's forwarded entry goes uncommitted for the hedge delay, it treats the leader as suspect. It hands nothing more to that leader and proposes its pending entries itself, until it sees a slot committed with that slot's leader's value.
- **Silence detection.** A follower suspects the leader as soon as the leader has sent it nothing for about one commit latency since it forwarded an entry, instead of after four commit latencies.

- **Faster takeover.** The dead leader's unfinished slots are taken over after σ instead of after (position + 1) × δ.

None of these touches who may decide a slot, the leader schedule, or durability. They change only when a follower stops waiting.

### Problem Frame

The cross-region baseline (`docs/benchmarks.md`, "Across regions") shows QuePaxa failing no request but stalling 3.2 s (3,204-3,453 ms) in every surviving region when the leader's region is lost. That is longer than default etcd's election, about 2 s. Its p99 there is 7-8 s, so the backlog drains slowly after the stall.

Reading the code gives three hypotheses, which U1 must confirm or replace. The first is the likeliest main cause of the stall:

- **The stall is the takeover of the dead leader's in-flight slots.**
  - Under load, the slots from `committed` up to the in-flight window are the leader's own.
  - Every follower already holds a register for them as a recorder, so `alloc_slot` skips them.
  - Commits are delivered in slot order (`emit_commits`), so nothing later reaches clients until they decide.
  - Only `stalled()` takes them over, after (position + 1) × δ: 2δ for the first follower in line. With δ ≈ 1.6 s, that is the observed 3.2 s.

- **Forwarded entries wait for the hedge delay.** A follower forwards entries to the leader and hedges an entry only after δ = 4 × its smoothed commit latency, capped at `hedge_max` (`replica/proposing.rs`, `hedge_delay`). Across regions a follower's commit latency through a far leader is 450-650 ms, so δ is 1.8-2.6 s.
- **The drain is slow because the leader is not replaced for a long time.** The leader rotates out only after 8 consecutive slots decided without its value, and the new order takes effect only `LAG` = 64 slots later (`schedule.rs`). With 8 slots in flight and WAN round trips, that takes seconds. Until then a follower keeps forwarding every new entry to the dead leader (`forward_pending`), and each one waits a fresh δ. A hedged batch that loses its slot is also handed back to the leader again (`lost_slot`).

This matters because cross-region availability is the library's stated strength (`STRATEGY.md`), and today the one fault that matters most across regions is where QuePaxa trails etcd.

### Requirements

**Recovery**

- R1. With the leader's region crashed or partitioned, surviving regions' longest write gap is at most about 2 × their smoothed commit latency. On `wan.sh`, that is under 1.5 s at the baseline matrix.
- R2. After the gap, surviving regions' p99 over the run falls well below today's 7-8 s, and the post-fault backlog drains within about 3 s.
- R3. No client request fails or goes unanswered because of these changes, in any `wan.sh` or `run.sh` case.

**No regression**

- R4. With a healthy leader, `wan.sh` no-fault (far and best-placed) p99 and `run.sh` steady state stay within their published ranges.
- R5. With a slow or jittery leader that is still alive, `wan.sh` slow and jitter and `run.sh` slow-disk and far-leader do not get worse in p99 or longest gap.
- R6. Every existing verification layer still passes:
  - TLA+ checks and TLAPS proofs;
  - Verus proofs;
  - the simulator with its linearizability oracle;
  - the model checker;
  - all canaries.

**Evidence**

- R7. Before/after numbers for `wan.sh` (all cases) and `run.sh` (fault cases) are published in `docs/benchmarks.md`, with what changed and why.

### Scope Boundaries

- Not changed: the leader schedule (`ROTATE_AFTER`, `LAG`, the window rule), slot decision, recorder behavior, durability gating, the wire format, the public API, and `Config` defaults other than documentation. (KTD5)
- Not done: lowering the hedge multiplier globally. Raising it to 4× fixed routine hedging under a healthy leader's tail (#121).
- Not done: a leader lease or any timeout that blocks progress. A wrong suspicion costs only contention, never availability or safety.
- Considered and not built: a separate heartbeat message. The leader already talks to every follower whenever there is work. Revisit if U3/U4's healthy-leader hedge counts show silence detection misfiring at low load.

#### Deferred to Follow-Up Work

- Faster leader rotation, which changes the verified schedule.
- Capping how many entries a follower hedges at once. It was considered in #123 and may become relevant if U2's contention shows up in `run.sh` slow-disk.

---

## Planning Contract

### Key Technical Decisions

- KTD1. **Suspect the leader after a hedge.** The first forwarded entry a follower hedges puts it in suspect mode for the current leader. On entry, every entry still in `forwarded` to that leader moves to `hedged` at once: suspicion is evidence about the leader, and it covers what was handed to it. In suspect mode:
  - `drive` does not call `forward_pending`;
  - `cut_batch` takes pending entries as if leading;
  - `lost_slot` keeps a batch for re-proposal instead of handing it back to the leader.

  This removes the per-entry δ wait during the rotation window, which is where the slow drain comes from.
- KTD2. **Leave suspect mode on evidence the leader is alive now.** A follower leaves suspect mode when a message from the suspected leader arrives after it entered suspect mode (KTD4's last-heard time). It also leaves when the leader of `Slot(committed)` changes, so after a rotation it forwards to the new leader without a timer.

  A slot decided with the leader's value is not that evidence. A dead leader's in-flight slots, taken over by followers, still decide with its value, because phase 0 adopts the highest-priority value a quorum recorded.
- KTD3. **Detect a silent leader early, and take over its slots sooner.**
  - A forwarded entry is hedged at the earlier of two times:
    - δ after it was forwarded, as today;
    - σ after it was forwarded, if nothing at all has arrived from the leader since the forward.
  - `stalled()` takes over an undecided slot whose leader is suspected, or has been silent since the slot was seen, after position × σ instead of (position + 1) × δ. The first follower in line takes it over after σ. This is the takeover the stall is made of (Problem Frame).
  - σ adapts to how long this follower waits for the leader's first message after a forward when the leader is healthy. A smoothed estimate of that delay R is kept, and σ = clamp(max(smoothed commit latency, 2 × R), `hedge_min`, δ).
    - Commit latency alone could be a few ms on a flat network, while a slow-disk leader holds its first requests in the durability gate until its reservation is durable.
    - R takes in that wait, so a leader slowed by its disk is not mistaken for a dead one.

  An alive leader that received the forward sends this follower a record request within R. So silence for σ is strong evidence of a dead or cut-off leader, while a slow but alive leader keeps sending and is waited for as before. "Since the forward" makes an idle cluster harmless: an earlier quiet period does not count.
- KTD4. **Last-heard bookkeeping lives in `on_message`.** The time a message last arrived from each member is recorded there, for any body type. A message from the leader is evidence it is alive whatever it carries. Nothing new goes on the wire.
- KTD5. **Timers only, so safety arguments are untouched.** All changes sit on the follower's decision of when to propose itself. Any replica may propose in any slot, already safe and exercised by hedging, so the TLA+ models, TLAPS proofs (which model no timers) and Verus proofs (gate, dedup, schedule) need no change. This rests on not touching `schedule.rs`, `gate.rs` or `dedup.rs`. A change that needs them stops the plan.

### High-Level Technical Design

```mermaid
stateDiagram-v2
  [*] --> Forwarding
  Forwarding --> Suspect: a forwarded entry hedges (δ elapsed, or σ elapsed with the leader silent since the forward)
  Suspect --> Forwarding: a message from the leader after suspicion, or the leader changes
  note right of Forwarding: pending entries are forwarded to the leader; hedge at δ, or σ if silent
  note right of Suspect: forwarded entries hedge at once; pending entries are proposed directly; lost slots are re-proposed; the leader's stalled slots are taken over after position x sigma
```

The follower is in Suspect for a given leader. If the leader changes, the state starts again as Forwarding toward the new one.

### Assumptions

- Under load, an alive leader sends each follower a record request within about one round trip after receiving its forward, plus the leader's durable reservation wait. U1 measures this. If it fails, R5 for slow disk is at risk and the stop condition applies.
- Contention from several followers proposing directly in suspect mode resolves through QuePaxa's randomized priorities fast enough to meet R2. U1 and U5 measure this.

---

## Implementation Units

### U1. Reproduce and attribute the stall in the simulator

- **Goal:** a deterministic simulator scenario that measures the gap and the drain after the leader dies, and a breakdown of where the time goes.
- **Requirements:** R1, R2 (baseline), stop condition 1
- **Dependencies:** none
- **Files:**
  - `crates/quepaxa-sim/tests/replica_sim.rs` (or a new `crates/quepaxa-sim/tests/leader_loss.rs`);
  - `crates/quepaxa-sim/src/config.rs`, `crates/quepaxa-sim/src/lib.rs` and `crates/quepaxa-sim/src/actions.rs`. The report needs each commit's time and proposing node, because `max_commit_gap_ms` is cluster-wide and `commit_latencies_us` carries no time or region.
- **Approach:**
  1. Use the `wan_leaders` five-region matrix (`links_ms`), the leader pinned to the farthest region, steady proposals and `kill` of the leader at a fixed time.
  2. Report the longest gap after the kill and the p99 of commit latency afterwards.
  3. Per run, also report from metrics and the trace:
     - when slot `committed` is taken over after the kill, and by whom;
     - time to first hedge;
     - entries hedged, and entries forwarded after the kill;
     - when the rotation takes effect.
  4. Record the baseline numbers in the test's comment so U2-U3 can assert against them.
- **Execution note:** characterization first. This unit changes no engine code.
- **Test scenarios:**
  - Leader killed at t = 10 s, five regions, three seeds: the gap is close to δ plus one commit, and entries forwarded after the kill each wait about δ. This confirms or refutes the diagnosis.
  - The same scenario with the leader killed in the best-placed region: the gap scales with that region's commit latency.
  - A healthy leader, idle, at low load and with a slow persist on the leader (`max_persist_ms`): the delay from a follower's forward to the leader's first message to it. This is R in KTD3, and it shows whether the slow-disk risk is real.
- **Verification:** the numbers and the attribution are written down. If they contradict the Problem Frame, stop.

### U2. Suspect mode

- **Goal:** a follower that has hedged stops forwarding to that leader and proposes directly until the leader is seen to work.
- **Requirements:** R2, R3 (KTD1, KTD2)
- **Dependencies:** U1
- **Files:**
  - `crates/quepaxa/src/replica/proposing.rs`;
  - `crates/quepaxa/src/replica/mod.rs` (state);
  - `crates/quepaxa/src/replica/log.rs` (the exit signal at decision time);
  - `crates/quepaxa/src/replica/tests.rs`.
- **Approach:**
  - Hold the suspected leader as an optional `NodeId` on the replica, cleared when it no longer leads `Slot(committed)`.
  - Enter suspect mode where a forwarded entry is moved to `hedged` (`fire_timers`).
  - Leave it in the decide path when the decided value's origin equals the slot's leader.
  - `leads_now` callers that gate forwarding and batching consult "leading or suspecting".
  - The `model` feature's `Hash` derive covers the new field.
- **Patterns to follow:** the existing `a_follower_hands_a_hedged_batch_that_lost_its_slot_back_to_the_leader` and `a_hedged_entry_does_not_stretch_the_hedge_delay` tests in `replica/tests.rs`.
- **Test scenarios:**
  - A follower whose forwarded entry hedges: a new proposal afterwards is put in a slot of its own, not forwarded.
  - In suspect mode a hedged batch that loses its slot is proposed again in another slot, not handed back.
  - A slot decides with the leader's own value: the follower forwards the next proposal again.
  - The schedule rotates to a new leader: the follower forwards to the new leader without a hedge.
  - On entering suspect mode, an entry forwarded just before is proposed at once, not after its own δ.
  - A dead leader's recorded slot is taken over and decides with the leader's value: the follower stays in suspect mode.
  - A message from the suspected leader arrives: the follower leaves suspect mode and forwards again.
  - Every entry still commits exactly once (dedup), across suspect entry and exit.
- **Verification:** the new tests pass, together with all `replica` unit tests and the `tests/` integration suites.

### U3. Early detection of a silent leader

- **Goal:** hedge after about one commit latency when the leader has said nothing since the forward.
- **Requirements:** R1, R4, R5 (KTD3, KTD4)
- **Dependencies:** U2
- **Files:**
  - `crates/quepaxa/src/replica/messages.rs` (last-heard);
  - `crates/quepaxa/src/replica/proposing.rs` (`earliest_timer`, `fire_timers`, `stalled`, σ, R);
  - `crates/quepaxa/src/replica/mod.rs`;
  - `crates/quepaxa/src/replica/tests.rs`.
- **Approach:**
  1. Record the arrival time per known sender in `on_message`.
  2. For each forwarded entry, its early deadline is forward time + σ. It applies only if the leader's last-heard time is before the forward time.
  3. Update R when the leader's first message after a forward arrives. Keep it with the same smoothing as the commit latency.
  4. `stalled` uses position × σ for slots whose leader is suspected or silent since the slot was seen.
  5. `earliest_timer` includes both early deadlines, so a sans-IO driver wakes for them.
- **Test scenarios:**
  - The leader is silent after a forward: the entry hedges at σ, not δ.
  - The leader sends any message (a record request for another slot) after the forward: the entry waits for δ as today.
  - An idle cluster where the leader has been quiet for minutes, then a forward: the entry does not hedge before σ, and not at all when the leader replies in time.
  - σ never exceeds δ and never drops below `hedge_min`, including before any commit latency is known.
  - The leader dies with requests for its own slots delivered: those slots are taken over and decided within about 2σ, not 2δ.
  - A leader whose first message after a forward is held by a slow persist (R large): the entry waits, and σ is at least 2R.
- **Verification:** the unit tests pass. U1's scenario now shows a gap of about σ plus one commit.

### U4. Verification layers

- **Goal:** every existing layer passes, and the new behavior has a guard that catches its plausible bugs.
- **Requirements:** R3, R6
- **Dependencies:** U2, U3
- **Files:**
  - `crates/quepaxa-sim/tests/` (the U1 scenario becomes an assertion);
  - `verification/canaries/` (new patches) and `verification/canaries.tsv`;
  - `VERIFICATION.md` (model-to-code note: timers are outside the models; the new state is liveness-only).
- **Approach:**
  - Turn U1's scenario into a test asserting R1's bound with margin across seeds.
  - Add canaries:
    - suspect mode that still forwards (the drain bound fails);
    - early detection that ignores "since the forward" (healthy-leader hedges appear in a no-fault run, caught by a hedge-count bound under a healthy leader).
  - Run the TLA+ checks and TLAPS proofs as a regression check, even though no model changes.
- **Test scenarios:**
  - Leader killed across regions: the gap is under 2 × commit latency for every seed.
  - Healthy leader across regions, no fault: no entry hedges (or a small bounded number) over the run.
  - The existing hostile simulator seeds (crashes, partitions, drops): linearizability holds and runs heal within budget.
- **Verification:**
  - `cargo test --workspace` passes, including the simulator and the model checker.
  - `verification/canaries.sh` catches every canary.
  - `verification/tla/run.sh` passes, and `prove.sh` reports all obligations proved.

### U5. Before/after measurements and docs

- **Goal:** published evidence that the stall and the drain shrank and nothing else got worse.
- **Requirements:** R1, R2, R4, R5, R7
- **Dependencies:** U4
- **Files:**
  - `docs/benchmarks.md`;
  - `crates/quepaxa/src/guide/` (the tuning guide's hedge section);
  - `crates/quepaxa/src/api.rs` (`hedge_max` doc, if its meaning shifts);
  - `CHANGELOG.md`.
- **Approach:**
  - Run `wan.sh` (all cases, `REPEATS=6`) and `run.sh` fault cases on a quiet host.
  - Compare with the published baseline: same matrix, rates and timers.
  - Report gaps, p99s, errors and hedge counts.
  - Describe the change in the guide's hedging section.
  - Add one CHANGELOG line.
- **Test expectation:** none (measurement and docs).
- **Verification:** R1, R2, R4 and R5 hold in the published numbers, or the docs say plainly which did not and why.

---

## Risks & Dependencies

| Risk | Mitigation |
|---|---|
| A slow-disk leader is silent long enough to trip early detection, so followers hedge and contend (the #123 storm) | KTD3 measures silence since the forward against a whole commit latency; `run.sh` slow-disk is a stop condition (R5) |
| Several followers in suspect mode contend for the same slots and drain slowly | U1/U5 measure drain; randomized priorities already resolve hedge contention; deferred cap on simultaneous hedging if needed |
| A suspected-but-alive leader (partition heals) loses slots to followers for a while | KTD2 exits on the first slot it decides; correctness unaffected (KTD5) |
| Model checker state space grows with the new fields | Fields are small; if exhaustive runs slow down, the existing bounds apply |

---

## Verification Contract

| Gate | Command | Applies to |
|---|---|---|
| Unit and integration tests | `cargo test --workspace` | U1-U4 |
| Lint | `cargo clippy --workspace --all-targets --all-features`, `cargo fmt --all --check` | U2-U4 |
| Canaries | `verification/canaries.sh` | U4 |
| Models and proofs | `verification/tla/run.sh`, `verification/tla/prove.sh`, `verification/verus/` scripts | U4 (regression) |
| Cross-region | `examples/kv-tokio/bench/etcd/wan.sh` (`REPEATS=6`) | U5 |
| Flat-network faults | `examples/kv-tokio/bench/etcd/run.sh crash partition slow-disk far-leader` | U5 |

---

## Definition of Done

- R1-R7 hold, with numbers in `docs/benchmarks.md`.
- All gates pass. CI is green, and the Codex and cross-model reviews are handled.
- `schedule.rs`, `gate.rs` and `dedup.rs` are unchanged.
- No abandoned approaches remain in the diff.

---

## Sources & Research

- `crates/quepaxa/src/replica/proposing.rs`:
  - `hedge_delay` is 4 × the smoothed latency, clamped;
  - `forward_pending`;
  - `lost_slot` hands batches back to the leader;
  - `fire_timers` moves forwarded entries to `hedged`;
  - `stalled` drives others' slots after (position + 1) · δ.
- `crates/quepaxa/src/schedule.rs`: `ROTATE_AFTER` = 8, `LAG` = 64.
- `crates/quepaxa/src/replica/messages.rs` `on_message`: the single entry point for peer messages.
- `crates/quepaxa-sim`: `links_ms` per-link latency, `kill` at a time, `wan_leaders` matrix.
- `docs/benchmarks.md`, "Across regions" and "Leader faults": the baseline and the #121/#123 history of the hedge delay.
- `STRATEGY.md`, cross-region track: the motivation.

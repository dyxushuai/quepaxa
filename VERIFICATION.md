# Verification

How `quepaxa`'s correctness is checked, and how to keep that checking strong as the code
changes. The protocol is the paper's (QuePaxa, SOSP'23); the invariants below are its
lemmas, checked on the real code — there is no second model to drift from it.

## Layers

| Layer | Where | Checks | Runs |
|---|---|---|---|
| Unit and property tests | `crates/quepaxa/src/**` (`#[cfg(test)]`), `crates/quepaxa/tests/` | Alg. 3/4 worked examples, recorder invariants R1–R6, ordering, codec round-trips and golden bytes, API contracts, adversarial member messages | every PR |
| Single-slot simulation | `crates/quepaxa/src/slot_sim.rs` | G1–G5 on proposer/recorder state after every event under loss, duplication, reordering, partitions | every PR |
| Replica simulation | `crates/quepaxa-sim` (`tests/replica_sim.rs`) | agreement, validity, at-most-once, ordered commits, KV linearizability, convergence after heal, and **G1/G3/G4/G5 on every replica's recorder state after every event**; crash-restart losing unsynced or torn writes; snapshots | every PR (40 seeds), nightly (100k seeds) |
| Model checking | `crates/quepaxa-sim/src/model.rs` (`tests/model_check.rs`) | every schedule within *k* deviations from a fair default (reorder, loss, duplication, early timer, partition) plus crash-restarts losing any suffix of the write in progress, with client retries: the same invariants in every state, liveness after the partition heals | every PR (exhaustive), nightly (+1 deviation, exhaustive) |
| Fuzzing | bolero targets in `crates/quepaxa/src` | decoding of messages, batches, durable records, core state | corpus replay every PR, 20 min/target nightly |
| Unsafe code | `crates/quepaxa-ffi` | Miri; ASan/UBSan on the C smoke test | every PR |
| Mutation testing | `.cargo/mutants.toml` | test strength over the whole workspace | weekly |
| **Seeded-bug canaries** | `verification/` | each known class of bug is still caught by the check meant to catch it | every PR |

The invariants (`crates/quepaxa-sim/src/invariants.rs`, from `Replica::inspect` under the
test-only `model` feature):

- **G1 agreement** — a slot is only ever decided to one value.
- **G3 value lock** — after a round-*r* decision of *v*, every value stored at step ≥ 4(*r*+1) is *v*.
- **G4 fast-path lock** — a majority holding the leader's top-priority proposal first at step 4 fixes the decision.
- **G5 unique H** — at most one value ever carries the leader priority per slot.

Evidence accumulates across steps; a crash forgets only what the crashed replica's
volatile state showed (its replies wait for durability, so that state never had an effect).

## Running it locally

```sh
cargo nextest run --workspace                                   # everything per-PR
cargo nextest run --release -p quepaxa-sim --test model_check   # model checking (release)
QPX_SIM_SEEDS=100000 cargo nextest run --release -p quepaxa-sim --test replica_sim
QPX_MODEL_EXTRA_DEVIATIONS=1 cargo nextest run --release -p quepaxa-sim --test model_check
verification/canaries.sh [name]                                 # seeded bugs
cargo +nightly bolero test -p quepaxa 'codec::tests::decode_never_panics_on_arbitrary_input' --target x86_64-unknown-linux-gnu -T 60s
```

A simulator failure prints its seed; a model-checking failure prints the schedule (set
`QPX_MODEL_REPLAY=1` to replay it step by step).

## Keeping it strong

Verification decays silently: an oracle gets weaker, a scenario stops reaching the code
that matters, and every test stays green. The canaries make that loud.

- `verification/canaries/*.patch` each plant one known bug; `verification/canaries.tsv`
  names the check that must catch it and the expected failure. `verification/canaries.sh`
  fails if a canary is **missed** (verification got weaker) or **stale** (the patch no
  longer applies because the code moved — refresh it against the new code).
- When you fix a correctness bug, add a canary that re-introduces it.
- When you add protocol logic, add or extend an invariant, then a canary proving the
  invariant catches a broken version.
- When you change an oracle, run the canaries: a stronger oracle may catch a canary with a
  different message; update the expected pattern, never drop the canary.

Current canaries: skipped phase-3 adoption, deciding on any matching aggregate, local or
remote replies before persisting, sends before the incarnation is durable, commit-time
dedup disabled, checkpoints dropping recorder state, read barriers answered locally, and a
restarted leader reusing its top priority.

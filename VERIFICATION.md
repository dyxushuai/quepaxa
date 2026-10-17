# Verification

How `quepaxa`'s correctness is checked, and how to keep that checking strong as the code
changes. The protocol is the paper's (QuePaxa, SOSP'23); the invariants below are its
lemmas, checked on the real code — there is no second model to drift from it.

## Layers

| Layer | Where | Checks | Runs |
|---|---|---|---|
| Unit and property tests | `crates/quepaxa/src/**` (`#[cfg(test)]`), `crates/quepaxa/tests/` | Alg. 3/4 worked examples, recorder invariants R1–R6, ordering, codec round-trips and golden bytes, API contracts, adversarial member messages | every PR |
| Single-slot simulation | `crates/quepaxa/src/slot_sim.rs` | G1–G5 on proposer/recorder state after every event under loss, duplication, reordering, partitions | every PR |
| Replica simulation | `crates/quepaxa-sim` (`tests/replica_sim.rs`) | agreement, validity, at-most-once, ordered commits, KV linearizability, convergence after heal, and **G1/G3/G4/G5 on every replica's recorder state after every event**; crash-restart losing unsynced or torn writes; snapshots; membership changes and retirement | every PR (40 seeds; membership 120), nightly (100k seeds, 8 shards) |
| Model checking | `crates/quepaxa-sim/src/model.rs` (`tests/model_check.rs`) | every schedule within *k* deviations from a fair default (reorder, loss, duplication, early timer, partition) plus crash-restarts losing any suffix of the write in progress, with client retries: the same invariants in every state, liveness after the partition heals | every PR (exhaustive), nightly (+1 deviation, exhaustive) |
| Independent models | `verification/tla/` (TLA+, TLC) | `QuePaxa.tla`: one slot of the paper's algorithms written apart from the Rust code, Agreement and Validity over random behaviours with loss and reordering. `Membership.tla`: changes across slots, crash-recovery and retirement, **exhaustively** for 4 replicas and 3 slots, in two configurations (a change removing two of three members; a change to three members; 4.3M states each): every replica that may drive a slot computes its true members (the window rule), and no decided slot is lost when removed replicas retire, even with a further minority of the members down. Seven seeded bugs (two protocol, five membership) must be found | nightly (~35 min) |
| Trace validation | `verification/tla/QuePaxaTrace.tla`, `traces.sh`; logs from `crates/quepaxa/src/slot_sim.rs` | the real `Register` and `SlotProposer` under a hostile network (3 or 5 recorders, 2 or 3 proposers): every delivered message must be the spec's action for a message the spec allows to exist, leading to the logged state; 30 traces must all be accepted, and a planted deviation (phase 0 adopting the first reply) must be rejected | nightly; its canary every PR |
| Bounded proof | `#[cfg(kani)]` harness in `crates/quepaxa/src/recorder.rs` (Kani) | recorder invariants R1–R6 for one `record` from **every** register state, step and value, payload bytes included (the property test samples steps 4–11 and cannot see payloads, which equality ignores); a seeded bug (`verification/kani-canary.patch`) must fail it | nightly |
| Fuzzing | bolero targets in `crates/quepaxa/src` | decoding of messages, batches, durable records, core state | corpus replay every PR, 20 min/target nightly |
| Unsafe code | `crates/quepaxa-ffi` | Miri; ASan/UBSan on the C smoke test | every PR |
| Mutation testing | `.cargo/mutants.toml`, `verification/mutants-check.sh` | test strength over the whole workspace; fails on any missed mutant not in `verification/mutants-accepted.txt` | weekly |
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
QPX_SIM_SEEDS=100000 cargo nextest run --release -p quepaxa-sim --test replica_sim   # all cores; QPX_SIM_SHARD=i/n for a slice
QPX_MODEL_EXTRA_DEVIATIONS=1 cargo nextest run --release -p quepaxa-sim --test model_check
verification/canaries.sh [name]                                 # seeded bugs
TLA_SECONDS=120 verification/tla/run.sh                          # TLA+ models, their seeded bugs, trace validation (Java or Docker)
verification/tla/traces.sh                                       # trace validation only
cargo kani -p quepaxa                                            # bounded proofs (cargo install --locked kani-verifier; cargo kani setup)
cargo mutants -p quepaxa -f crates/quepaxa/src/dedup.rs          # mutants of one file (whole workspace tested)
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
- Mutation testing ratchets: a missed mutant fails the weekly run unless
  `verification/mutants-accepted.txt` lists it with the reason it cannot or need not be
  killed (equivalent, liveness or performance only). Prefer a test; shrink the list when
  you can.
- When you change an oracle, run the canaries: a stronger oracle may catch a canary with a
  different message; update the expected pattern, never drop the canary.

Current canaries: skipped phase-3 adoption, deciding on any matching aggregate, local or
remote replies before persisting, sends before the incarnation is durable, commit-time
dedup disabled, checkpoints dropping recorder state, read barriers answered locally, a
restarted leader reusing its top priority, a membership change taking effect early or
applied twice, and proposers using the configured members instead of the slot's.

## Where Kani does not reach

Kani (CBMC) proves the recorder register in under a minute, but harnesses for the
durability gate (persist-before-send over 4–7 operations) and the leader schedule (the
64-slot window rule, by induction over one decided slot) did not finish in 10–20 minutes
and grew past 40 GB: their state is `Vec`s of `Vec`s, and symbolic copies, drains and
rotations dominate the formula. Those properties stay with the property tests, the
simulator and the model checker of the real replica; `Membership.tla` checks the schedule's
window rule and retirement exhaustively at the model level instead.


# Which tools give us model checking, DST and linearizability checking in Rust

Parent: [map](../map.md)
Type: research
Status: resolved
Blocked by: 

## Question

Survey and recommend: model checking/formal methods for a sans-IO Rust state machine (stateright, TLA+/Apalache incl. any existing QuePaxa TLA+ spec, Kani, Verus, P), deterministic simulation (madsim, turmoil, hand-rolled), linearizability checkers (porcupine-style, stateright's, knossos), fuzzing (cargo-fuzz/libFuzzer, AFL++, bolero, structure-aware fuzzing of message sequences), coverage (cargo-llvm-cov). For each: maturity, no_std friendliness, CI cost, how it would fit. Recommend one stack.

## Answer

Full survey: `docs/research/assurance-tooling.md` on branch `research/assurance-tooling` (commit 1a01b8c).

- Existing formal work: authors' Spin/Promela models in `dedis/quepaxa/model-checker/` (single slot, safety only, bitstate, non-exhaustive); unreviewed third-party `SorinS/quepaxa-tlaplus` (single slot). No public model of multi-slot, fast path, hedging or crash-restart.
- Recommended stack, all driving the one sans-IO core (no second model):
  - Unit/TDD: nextest + proptest + proptest-state-machine.
  - DST: hand-rolled `quepaxa-sim` (VOPR/FoundationDB style; madsim/turmoil don't apply to a sans-IO core — turmoil reserved for a future runtime crate).
  - Linearizability: stateright `LinearizabilityTester`, decomposed per slot as `WriteOnceRegister`.
  - Model checking: stateright 0.31 bounded exhaustive (N=3, few slots). Risk: 15 months without a release → keep adapter thin.
  - Optional bounded proofs: Kani via bolero harnesses.
  - Fuzzing: bolero 0.13 + arbitrary 1.5 (libFuzzer/AFL++/Kani, corpus replays on stable); targets: wire decode, single-node event sequences, simulator schedule choices.
  - Coverage: cargo-llvm-cov (signal, not gate). Mutation: cargo-mutants `--in-diff` per PR, sharded weekly.
- Gap: no unbounded safety proof; TLA+/Quint trace validation or Verus is a later decision.
- Found: crates.io `quepaxa` is owned by `johntaiko` (0.1.0, "fictional project", created 2026-07-10) — see the crate-name ticket.

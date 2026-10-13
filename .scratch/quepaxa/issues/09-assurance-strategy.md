# What is the assurance strategy

Parent: [map](../map.md)
Type: grilling
Status: resolved
Blocked by: 03, 06

## Question

Decide the concrete test/verification stack and its gates: unit/TDD conventions, proptest properties, fuzz targets, DST scenarios and fault schedules, linearizability checking, the chosen model checker and what it proves, coverage thresholds, and what runs per-PR vs nightly.

## Answer

Resolved 2026-10-13 under the "一步到位" directive. Stack from the assurance tooling research, all driving the one sans-IO core.

1. **TDD convention**: every backlog ticket starts with a failing test; recorder/proposer tables from the Go impl become table-driven unit tests; the paper's invariants are executable checkers: R1–R6 (recorder), P1–P6 (proposer) as `debug_assert`s, G1–G5 as simulator oracles.
2. **proptest** + `proptest-state-machine` for the recorder, proposal ordering, codec round-trip (`decode(encode(m)) == m`), durable-record replay equivalence.
3. **`quepaxa-sim`** (workspace, `publish = false` in v1): seeded single-thread simulator; faults: drop, duplicate, reorder, delay, partition/heal, crash-restart with disk losing unsynced writes and torn last write, `StorageFailed`, snapshot install. Liveness check: after heal, commits resume within a bound.
4. **Linearizability**: toy KV app in the sim using read barriers; histories checked with stateright `LinearizabilityTester`, decomposed per slot/key.
5. **Model checking**: stateright actor model wrapping the real `Replica`, N=3, bounded slots/steps, safety properties G1–G5, with crash-restart enabled (covers A15).
6. **Fuzzing**: bolero + arbitrary targets: wire decode, durable-record decode, single-replica event sequences, sim schedule choices. Corpus replays in `cargo test` on stable.
7. **Optional bounded proofs**: Kani on proposal ordering and codec length arithmetic.
8. **Unsafe code**: Miri on `quepaxa-ffi` tests; ASan/UBSan on the C smoke test.
9. **Gates**: per PR — fmt, clippy `-D warnings`, nextest (unit + proptest), DST 1 000 seeds, stateright small bound, fuzz corpus replay, `cargo-mutants --in-diff`, Miri, MSRV + `no_std` builds, cargo-deny. Nightly — fuzz 1 h/target, DST 1 M seeds, larger stateright bounds, Kani. Weekly — full sharded cargo-mutants. Coverage (cargo-llvm-cov) is reported, not gated.

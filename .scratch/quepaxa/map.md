# Map: quepaxa — open-source Rust QuePaxa

Labels: wayfinder:map

## Destination

An implementation-ready spec for the `quepaxa` crate family: verified protocol understanding, architecture + ADRs, assurance/test strategy, FFI and platform plan, and a TDD-ordered backlog of implementation tickets. Implementation itself is a separate effort.

## Notes

- Domain: crash-fault-tolerant consensus (QuePaxa, SOSP'23, n = 2f+1). Paper: https://bford.info/pub/os/quepaxa/quepaxa.pdf. Go reference impl: `dedis/quepaxa`. Industrial context: Cloudflare Meerkat (https://blog.cloudflare.com/meerkat-introduction/) — Rust, internal-only, DST + formal verification.
- Settled at charting (2026-10-13), all defaults accepted:
  - Scope: full multi-slot replicated log (leader fast path, hedging, adaptive timeouts as pluggable policy, batching/pipelining). CFT only.
  - Core exposes an opaque-bytes replicated log + thin `read_barrier`; KV/leases live above it.
  - Architecture: sans-IO deterministic core (`quepaxa`, `no_std + alloc`) + `quepaxa-ffi` (`cdylib`/`staticlib`, C header). See [ADR 0001](../../docs/adr/0001-sans-io-core.md).
  - Platforms: Tier 1 (full tests in CI) Linux/macOS/Windows × x86_64/aarch64; Tier 2 (cross-build + smoke) Android, iOS, `wasm32-unknown-unknown`, one `no_std` embedded target.
  - Assurance: TDD unit tests, proptest, cargo-fuzz, deterministic simulation (partition/reorder/drop/crash-restart), linearizability checker, one model-checking/formal method.
  - License `MIT OR Apache-2.0`.
- Skills every session should consult: `rust`, `rust-best-practices`, `tdd`, `domain-modeling` (GLOSSARY.md + docs/adr/), `preferences`.
- Research findings live as `docs/research/<name>.md` on local `research/<name>` branches (never merged, never pushed).
- Repo artifacts in English; conversation in 简体中文.

## Decisions so far

- [QuePaxa protocol, precisely](issues/01-paper-protocol-spec.md): proposer→recorder RPC over 4-phase rounds with a per-slot interval summary register; leader fast path at step 4; paper is crash-stop only and leaves durability, δ-tuning, reconfig, snapshots unspecified (A1–A16).
- [How do existing Rust consensus crates shape a sans-IO API](issues/04-sans-io-api-prior-art.md): pull loop `handle`/`poll_actions`, persist-before-send encoded structurally in `Action::Persist{then}`, injected `Timestamp`+seed; openraft#2118 is the cautionary bug.
- [What toolchain delivers the C ABI across our platform tiers](issues/05-ffi-cross-platform-toolchain.md): hand-written `extern "C"` + committed cbindgen header + cargo-c; catch_unwind per export; all 6 Tier 1 targets run natively on GH-hosted runners; MSRV 1.89 proposed; wasm gets wasm-bindgen/wasip2 later, not C ABI.
- [Which tools give us model checking, DST and linearizability checking in Rust](issues/03-assurance-tooling.md): one sans-IO core driven by proptest, hand-rolled DST, stateright (model check + linearizability), bolero fuzzing, cargo-mutants; only Promela/TLA+ single-slot models exist publicly.
- [What the Go reference implementation actually does](issues/02-go-reference-impl.md): faithful Alg. 3/4 but no durability, low-entropy priorities, static hedging δ, leader-order tuner via log pseudo-command; supplies test vectors, scenarios and benchmark baselines.
- [Who owns the `quepaxa` crate name on crates.io](issues/13-crate-name-ownership.md): owned by the user's `johntaiko` account; placeholder 0.1.0 exists, so first real release is > 0.1.0 and the placeholder gets yanked.
- [What is the core sans-IO API](issues/06-core-api-shape.md): single `Replica`, `handle`/`poll_actions` pull loop, structural `Action::Persist{then}` (ADR 0002), inline `Bytes` batches, core-side batching/pipelining, at-most-once per local `ProposalId`, injected time + fresh CSPRNG seed per start.
- [Are reconfiguration and snapshots in the first release](issues/07-phase-one-scope.md): snapshots/compaction in v1 (app-owned snapshot bytes); reconfiguration, dynamic join, witnesses/learners are phase 2 with `config_epoch` + `cluster_id` reserved in every message now.

## Not yet specified

- **Performance targets & benchmark methodology**: what to measure (throughput/latency under leader failure, WAN, contention), whether to compare against the Go impl or Raft crates.
- **Reference runtime crate** (`quepaxa-tokio` or similar): whether one ships at all, and its transport.
- **ValueId indirection** (LAN optimization): agree on fixed-size batch ids and disseminate batches separately; needs its own durability/catch-up/liveness story; revisit after benchmarks.
- **Phase 2: reconfiguration, dynamic join, witness/learner nodes**: protocol design (paper gives none; fn. 5 slot reset per epoch), after v1 tickets settle.
- **Release engineering**: MSRV, semver/stability policy for the C ABI, docs/examples, supply-chain gates (`cargo-deny`/`cargo-vet`).
- **Final spec + TDD backlog ordering** (`to-spec` / `to-tickets`), once the decisions above land.

## Out of scope

- BFT variants — different protocol family; CFT only (charting decision).
- Application layers (KV store, leases, transactions) — built on top of the log, not in the crate.

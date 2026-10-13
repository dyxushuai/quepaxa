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

## Not yet specified

- **Performance targets & benchmark methodology**: what to measure (throughput/latency under leader failure, WAN, contention), whether to compare against the Go impl or Raft crates.
- **Reference runtime crate** (`quepaxa-tokio` or similar): whether one ships at all, and its transport.
- **Adaptive hedging default policy**: which bandit/tuner ships as default and how it is tested deterministically.
- **Reconfiguration & bootstrapping, snapshots/log compaction**: designs, once their phase is decided.
- **Release engineering**: MSRV, semver/stability policy for the C ABI, docs/examples, supply-chain gates (`cargo-deny`/`cargo-vet`).
- **Final spec + TDD backlog ordering** (`to-spec` / `to-tickets`), once the decisions above land.

## Out of scope

- BFT variants — different protocol family; CFT only (charting decision).
- Application layers (KV store, leases, transactions) — built on top of the log, not in the crate.

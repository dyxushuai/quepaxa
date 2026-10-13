# Map: quepaxa — open-source Rust QuePaxa

Labels: wayfinder:map

## Destination

An implementation-ready spec for the `quepaxa` crate family: verified protocol understanding, architecture + ADRs, assurance/test strategy, FFI and platform plan, and a TDD-ordered backlog of implementation tickets. Implementation itself is a separate effort.

## Notes

- **Status (2026-10-13)**: destination reached and v1 implemented ([#1](https://github.com/dyxushuai/quepaxa/issues/1), milestone `v0.2.0`). Deviations from these decisions made during implementation are listed in [spec §12](../quepaxa-v1/spec.md#12-amendments-made-during-implementation); this map remains the decision record.

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
- **User directive (2026-10-13, "一步到位")**: remaining tickets are resolved by the agent's recommendations without per-round HITL; the user reviews the result afterwards. Any decision can be reopened by reopening its ticket.
- Repo artifacts in English; conversation in 简体中文.

## Decisions so far

- [QuePaxa protocol, precisely](issues/01-paper-protocol-spec.md): proposer→recorder RPC over 4-phase rounds with a per-slot interval summary register; leader fast path at step 4; paper is crash-stop only and leaves durability, δ-tuning, reconfig, snapshots unspecified (A1–A16).
- [How do existing Rust consensus crates shape a sans-IO API](issues/04-sans-io-api-prior-art.md): pull loop `handle`/`poll_actions`, persist-before-send encoded structurally in `Action::Persist{then}`, injected `Timestamp`+seed; openraft#2118 is the cautionary bug.
- [What toolchain delivers the C ABI across our platform tiers](issues/05-ffi-cross-platform-toolchain.md): hand-written `extern "C"` + committed cbindgen header + cargo-c; catch_unwind per export; all 6 Tier 1 targets run natively on GH-hosted runners; MSRV 1.89 proposed; wasm gets wasm-bindgen/wasip2 later, not C ABI.
- [Which tools give us model checking, DST and linearizability checking in Rust](issues/03-assurance-tooling.md): one sans-IO core driven by proptest, hand-rolled DST, stateright (model check + linearizability), bolero fuzzing, cargo-mutants; only Promela/TLA+ single-slot models exist publicly.
- [What the Go reference implementation actually does](issues/02-go-reference-impl.md): faithful Alg. 3/4 but no durability, low-entropy priorities, static hedging δ, leader-order tuner via log pseudo-command; supplies test vectors, scenarios and benchmark baselines.
- [Who owns the `quepaxa` crate name on crates.io](issues/13-crate-name-ownership.md): owned by the user (transferred to `dyxushuai`); placeholder 0.1.0 exists, so first real release is > 0.1.0 and the placeholder gets yanked.
- [What is the core sans-IO API](issues/06-core-api-shape.md): single `Replica`, `handle`/`poll_actions` pull loop, structural `Action::Persist{then}` (ADR 0002), inline `Bytes` batches, core-side batching/pipelining, at-most-once per local `ProposalId`, injected time + fresh CSPRNG seed per start.
- [Are reconfiguration and snapshots in the first release](issues/07-phase-one-scope.md): snapshots/compaction in v1 (app-owned snapshot bytes); reconfiguration, dynamic join, witnesses/learners are phase 2 with `config_epoch` + `cluster_id` reserved in every message now.
- [What is the wire format and its compatibility policy](issues/08-wire-format.md): core-owned hand-rolled LE codec for messages and CRC32C'd durable records; envelope with cluster_id/config_epoch/commit_index; exact-match wire version in 0.x, N−1 interop from 1.0.
- [What is the assurance strategy](issues/09-assurance-strategy.md): invariant checkers R/P/G, proptest, `quepaxa-sim` DST with disk faults, stateright model check with crash-restart, linearizability, bolero fuzz, Miri; per-PR / nightly / weekly gates.
- [What does the C ABI expose](issues/10-ffi-surface.md): opaque handle, `qpx_poll` tagged actions with borrowed bytes valid until next call, status codes + poisoning, ABI major/minor; C example only in v1.
- [What are the CI matrix, MSRV and release policy](issues/11-platform-ci-release.md): 6 native Tier 1 runners + 4 Tier 2 jobs, MSRV 1.89, first release 0.2.0 via trusted publishing, cargo-deny; criterion + e2e benchmarks; `examples/kv-tokio` as reference runtime.
- [What must persist, and how does a replica recover from crash-restart](issues/12-durability-recovery.md): replies released only after the state they report is durable; leader-H reservation + incarnation; durable Decided before Commit; Decided broadcast + CatchUp; A1/A2/A12/A13 closed.
- [How are the hedging delay and leader order chosen](issues/14-hedging-leader-policy.md): schedule as log entry effective W slots later; corrected epoch tuner (2n explore, ascending); adaptive δ; per-step progress evidence; forward-to-leader + commit-time dedup.
- [How do compaction and snapshot catch-up work safely](issues/15-compaction-snapshots.md): recorder state dropped once Decided is durable; app snapshot + `core_state()` then `compact`; `SnapshotNeeded`/`install_snapshot`.

## Not yet specified

_(empty — the way to the destination is clear; see [the v1 spec](../quepaxa-v1/spec.md))_

## Out of scope

- BFT variants — different protocol family; CFT only (charting decision).
- Application layers (KV store, leases, transactions) — built on top of the log, not in the crate.
- Phase 2: reconfiguration, dynamic join, witness/learner nodes — redrawn out of this map on 2026-10-13 ("一步到位"); v1 reserves `cluster_id`/`config_epoch`/per-epoch slots so it needs no wire break. A future effort.
- ValueId indirection (agree on batch ids, disseminate separately) — a performance effort to revisit with benchmark data.
- Published runtime crate (`quepaxa-tokio`), Kotlin/Swift/Python packages, wasm bindings — later efforts.

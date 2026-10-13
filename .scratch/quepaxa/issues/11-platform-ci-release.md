# What are the CI matrix, MSRV and release policy

Parent: [map](../map.md)
Type: grilling
Status: resolved
Blocked by: 05, 13

## Question

Decide the exact CI target matrix per tier, MSRV, semver policy for the Rust API vs C ABI, publish workflow to crates.io, and supply-chain gates.

## Answer

Resolved 2026-10-13 under the "一步到位" directive.

1. **CI matrix**: Tier 1 native full tests on `ubuntu-24.04`, `ubuntu-24.04-arm`, `windows-2025`, `windows-11-arm`, `macos-15` (arm64), `macos-15-intel` (until the image is retired, 2027-08). Tier 2 jobs: Android (cargo-ndk, NDK r28+, emulator smoke), iOS (xcframework, simulator smoke), `wasm32-unknown-unknown` (core build), `thumbv7em-none-eabihf` (core `no_std` build). MSRV, header/symbol diff, semver checks on `ubuntu-24.04`.
2. **Toolchain**: edition 2024, MSRV `rust-version = "1.89"`, bumped only in a minor release with a changelog note.
3. **Semver**: Rust API checked with cargo-semver-checks; C ABI versioned separately (see FFI ticket); wire and durable formats per the wire-format ticket.
4. **Crates**: `quepaxa` (core), `quepaxa-ffi` (published so it builds from source; also shipped as prebuilt artifacts), `quepaxa-sim` (`publish = false` in v1). Reserve `quepaxa-ffi` on crates.io when the first release lands.
5. **Release**: first real release `0.2.0` (0.1.0 placeholder exists, now owned by `dyxushuai`) then yank 0.1.0. Publish from GitHub Actions on tag via crates.io trusted publishing; GitHub Releases carry headers + Tier 1 libs (Linux built with cargo-zigbuild against glibc 2.28), Android `.so` per ABI and an iOS xcframework. Needs a public GitHub repo — the user creates it at implementation time.
6. **Supply chain**: cargo-deny (licenses `MIT OR Apache-2.0`-compatible, advisories, bans, sources) required on every PR; cargo-vet deferred. New dependencies need a one-line justification in the PR.
7. **Performance (graduated from fog)**: criterion microbenchmarks for recorder `record`, codec, and the replica handle/poll loop (nightly, flag >10% regressions); end-to-end benchmarks with the example runtime on loopback and netem WAN profiles, using the Go repo's 17-byte KV Poisson open-loop workload. Numbers are hypotheses, not production claims.
8. **Reference runtime (graduated from fog)**: v1 ships `examples/kv-tokio` (TCP transport + fsync'd file storage + toy KV), not a published runtime crate; a published `quepaxa-tokio` is a later effort.

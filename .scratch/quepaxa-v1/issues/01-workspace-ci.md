# 01: Workspace skeleton with CI gates

**What to build:** A cargo workspace (`quepaxa` no_std+alloc, `quepaxa-ffi`, `quepaxa-sim`) that builds and runs an empty test suite under the per-PR gates on Linux x86_64.

**Blocked by:** None (can start immediately)

**Status:** ready-for-agent

Spec: [spec.md](../spec.md). Start every change from a failing test.

- [ ] edition 2024, rust-version 1.89, MIT OR Apache-2.0 files
- [ ] CI: fmt, clippy -D warnings, nextest, MSRV build, `no_std` build of core for thumbv7em-none-eabihf, cargo-deny
- [ ] README states scope and links spec

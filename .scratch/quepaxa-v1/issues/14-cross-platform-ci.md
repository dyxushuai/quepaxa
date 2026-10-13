# 14: Tier 1 and Tier 2 platform CI

**What to build:** The full test suite runs natively on all six Tier 1 runners; Tier 2 jobs build and smoke-test Android, iOS, wasm32 core and thumbv7em core.

**Blocked by:** 13

**Status:** ready-for-agent

Spec: [spec.md](../spec.md). Start every change from a failing test.

- [ ] Android emulator and iOS simulator run the C smoke test
- [ ] wasm32-unknown-unknown and thumbv7em build core only
- [ ] cargo-semver-checks runs on PRs

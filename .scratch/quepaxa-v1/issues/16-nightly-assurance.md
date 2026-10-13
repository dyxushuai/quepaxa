# 16: Nightly and weekly assurance jobs

**What to build:** Scheduled CI: fuzz 1h per target, DST 1M seeds, larger stateright bounds, Kani, full sharded cargo-mutants, coverage report.

**Blocked by:** 10, 11, 12

**Status:** ready-for-agent

Spec: [spec.md](../spec.md). Start every change from a failing test.

- [ ] failures open an issue with the reproducing seed/input
- [ ] coverage published, not gated

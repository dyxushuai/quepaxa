# 05: Single-slot proposer decides via fast and slow paths

**What to build:** The proposer runs Alg. 4 against in-memory recorders for one slot: leader fast path at step 4, leaderless rounds, catch-up to max s′, Decided adoption.

**Blocked by:** 02, 03

**Status:** ready-for-agent

Spec: [spec.md](../spec.md). Start every change from a failing test.

- [ ] go proposer step table as vectors
- [ ] P1–P6 as debug_asserts, exercised by tests
- [ ] explicit test for A14 (leader catch-up at step 4)

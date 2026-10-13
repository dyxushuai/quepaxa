# 06: Deterministic simulator proves single-slot safety

**What to build:** `quepaxa-sim` runs N replicas' proposers/recorders over a seeded faulty network for one slot and checks G1–G5 after every step.

**Blocked by:** 04, 05

**Status:** ready-for-agent

Spec: [spec.md](../spec.md). Start every change from a failing test.

- [ ] faults: drop, duplicate, reorder, delay, partition/heal
- [ ] failing seed reproduces byte-for-byte
- [ ] CI runs 1k seeds per PR

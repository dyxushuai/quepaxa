# 03: Proposal ordering and seeded priorities

**What to build:** Proposals order by (priority, value_key); priorities are drawn per recorder from a seeded ChaCha in [1, H−1]; H = u64::MAX reserved.

**Blocked by:** 01

**Status:** ready-for-agent

Spec: [spec.md](../spec.md). Start every change from a failing test.

- [ ] distinct value_keys never compare equal (proptest)
- [ ] same seed ⇒ same priority stream on 32- and 64-bit targets
- [ ] optional Kani harness for ordering totality

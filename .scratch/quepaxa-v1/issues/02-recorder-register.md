# 02: Recorder register matches Alg. 3

**What to build:** The crate-internal recorder register handles `record(s, v)` exactly as the paper's Alg. 3 over the spec's proposal order.

**Blocked by:** 01

**Status:** ready-for-agent

Spec: [spec.md](../spec.md). Start every change from a failing test.

- [ ] table-driven tests ported from the Go recorder transition table
- [ ] proptest asserts R1–R6 on random sequences
- [ ] stale (s < S) input changes nothing

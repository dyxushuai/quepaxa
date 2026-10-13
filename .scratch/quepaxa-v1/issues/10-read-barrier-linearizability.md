# 10: Read barriers with linearizability checking

**What to build:** read_barrier/ReadReady and a toy KV in the sim whose client histories pass the stateright LinearizabilityTester.

**Blocked by:** 08

**Status:** ready-for-agent

Spec: [spec.md](../spec.md). Start every change from a failing test.

- [ ] histories with crashes and partitions linearize
- [ ] a read never returns state older than a completed write

# 15: Reference runtime example and benchmarks

**What to build:** `examples/kv-tokio` runs a real cluster (TCP + fsync'd file storage + toy KV); criterion benches for record, codec and loop; e2e loopback and netem WAN runs with the 17-byte KV Poisson workload.

**Blocked by:** 09, 11

**Status:** ready-for-agent

Spec: [spec.md](../spec.md). Start every change from a failing test.

- [ ] kill -9 of any minority keeps the cluster committing
- [ ] bench results recorded as hypotheses with environment details
- [ ] nightly criterion flags >10% regressions

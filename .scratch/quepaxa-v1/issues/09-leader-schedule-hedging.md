# 09: Leader schedule, tuner and hedging

**What to build:** ScheduleChange entries effective W slots later; default epoch tuner (2n explore, ascending exploit); adaptive δ; per-step progress evidence; Forward to leader with hedged self-proposal.

**Blocked by:** 08

**Status:** ready-for-agent

Spec: [spec.md](../spec.md). Start every change from a failing test.

- [ ] sim: leader crash mid-run, commits resume within bound
- [ ] sim: minority egress-delay attack keeps throughput
- [ ] sim: tuner converges to the fastest member
- [ ] all proposers agree on each slot's leader (oracle G5)

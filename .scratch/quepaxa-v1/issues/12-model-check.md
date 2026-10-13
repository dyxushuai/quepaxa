# 12: Stateright model check of the real Replica

**What to build:** A stateright actor model wrapping `Replica` (N=3, bounded slots/steps, crash-restart) checks G1–G5 exhaustively within bounds.

**Blocked by:** 07

**Status:** ready-for-agent

Spec: [spec.md](../spec.md). Start every change from a failing test.

- [ ] small bound runs per PR within budget
- [ ] larger bound runs nightly
- [ ] a deliberately injected bug (e.g. reply before persist) is caught

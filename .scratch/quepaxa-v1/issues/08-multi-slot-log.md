# 08: Multi-slot log with batching, pipelining and catch-up

**What to build:** Proposals batch into slots within the in-flight window; decisions broadcast; commit index piggybacked; CatchUp/Entries; Commit actions in strict slot order after Decided is durable; commit-time dedup.

**Blocked by:** 07

**Status:** ready-for-agent

Spec: [spec.md](../spec.md). Start every change from a failing test.

- [ ] a local ProposalId appears in at most one Commit entry (sim oracle)
- [ ] lagging replica catches up after partition heal
- [ ] backpressure returns Busy at max_outstanding_proposals

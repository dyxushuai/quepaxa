# QuePaxa protocol, precisely

Parent: [map](../map.md)
Type: research
Status: resolved
Blocked by: 

## Question

Read the QuePaxa paper (and any extended/tech-report version) and produce an implementation-grade protocol description: roles (proposer/recorder/leader), per-slot round and phase structure, message types and fields, priority/randomness rules, the threshold-synchronous broadcast primitive, decision and commit rules, leader fast path, hedging and adaptive timeout tuning, batching/pipelining, safety and liveness invariants (as checkable predicates), recovery after crash-restart, what state must be durable before replying, and what the paper says (or omits) about reconfiguration and snapshots. Cite page/section for every claim.

## Answer

Full spec: `docs/research/paper-protocol-spec.md` on branch `research/paper-protocol-spec` (commit f5954b2). Source: the 27-page bford.info PDF (incl. appendices A–E); no arXiv/extended version exists.

- Each replica = active proposer + passive recorder; all traffic is proposer→recorder RPC. Leader = the only proposer allowed reserved priority `H`, at step 4 only.
- Logical clock `step = 4·round + phase`; each step is one RTT to a majority. Recorder state per slot is a constant-space interval summary register `(S, Fc, Ac, Ap)` with one op `record(s, v)`.
- Proposal = `⟨priority, proposer, value⟩` packed into one comparable integer; fresh random priority per recorder in phase 0. Fast path decides at step 4 if all quorum replies carry `H`; else decide at phase 2 when `p = max(a′)`.
- Safety needs priorities unpredictable to the network adversary (RNG requirement). 18 checkable invariants (R1–R6, P1–P6, G1–G6) + statistical liveness L1–L3 listed in the doc.
- Hedging = staggered proposer start `0, δ, 2δ…`; the paper gives no δ-tuning algorithm. The bandit picks the leader (explore round-robin epochs, then exploit).
- Paper is crash-stop only: durability/recovery, reconfiguration, snapshots, dedup, decision broadcast are not specified (ambiguities A1–A16 in the doc, incl. a likely typo `p_i`→`p_j` in Alg. 4).

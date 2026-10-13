# What the Go reference implementation actually does

Parent: [map](../map.md)
Type: research
Status: resolved
Blocked by: 

## Question

Study `dedis/quepaxa` (and related DEDIS repos): module structure, message/wire types, state per replica, how hedging and the bandit tuner are implemented, durability, any deviations from the paper, test and benchmark harness, known bugs/TODOs. Produce a deviation table paper-vs-code and list what is reusable as test vectors or benchmark baselines.

## Answer

Full note: `docs/research/go-reference-impl.md` on branch `research/go-reference-impl` (commit 4e077cf). Authoritative code `dedis/quepaxa@bd16e25` (last code change 9a55960, 2023-07); no Go unit tests, no issues.

- Recorder/proposer match the paper's Alg. 3/4; proposals ordered by `(priority, proposer_id, thread_id)`, value never compared; priorities from `math/rand` in `[1, 99990]` (not high-entropy).
- "Bandit" = 2n+1 round-robin epochs then sort replicas by mean epoch time (fastest first), agreed via an `"Epoch…"` pseudo-command in the log. Tunes leader order only; hedging δ is a static flag (default 5 s), delay `δ × position × (slot − committedIndex)`.
- No durability at all (in-memory, no WAL/fsync/restart/compaction/reconfig) → persistence contract must come from the paper's proofs.
- 20-row paper-vs-code deviation table; fragile spots: panics on liveness events, fixed 1M-slot array, trusted network lengths, possible double execution of a client batch.
- Reusable: recorder transition table and proposer step table as test vectors; 10-case integration grid, leader crash at t=15 s, seed-39 slow-proposer attack, tuner convergence as scenarios; 17-byte KV Poisson open-loop workload + paper numbers (584k cmd/s LAN, 303–473 ms recovery) as baselines.

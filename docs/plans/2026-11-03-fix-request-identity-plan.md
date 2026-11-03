---
title: Request Identity Fixes - Delivery Record
type: fix
date: 2026-11-03
status: in-review
---

# Request Identity Fixes - Delivery Record

This record reconstructs the agreed scope and verification evidence from the repair
session. It was added after implementation; it is not a claim that a written plan or
formal review existed before the code was committed.

## Scope

- Review base: `b3135e33902be45a5787e5eb52788dddfebe2ab3`.
- Implementation head: `9b44658e295dca19440068ac40d6fa56e158fcd6`.
- U1: `06feef69eec586872b2766818d7c524cd2aaeb4f` separates read and logged request ids.
- U2: `9b44658e295dca19440068ac40d6fa56e158fcd6` fixes client-result association.
- Existing leader-recovery work before the review base is outside this repair review.
- PR #132 tracks that earlier work and these repairs. The repairs and this record were
  published at `6ce43ef` on 2026-11-03; CI on the current head is tracked in the PR.

## Requirements and Acceptance

| Requirement | Acceptance evidence | State |
| --- | --- | --- |
| R1: Reads must not create gaps in the logged request sequence. | `request_ids::reads_do_not_make_compacted_dedup_state_grow_with_completed_writes`; periodic compaction across 1,000 writes with frequent or isolated reads. | Implemented and tested |
| R2: Read and logged request ids must not collide or wrap within an incarnation. | Private boundary tests cover the last write/read ids, independent exhaustion, recovery, and reconfiguration; C ABI test checks the unsigned high bit. | Implemented and tested |
| R3: Existing durable and wire encodings must remain valid. | Recovery and snapshot-install tests preserve legacy holes and high write ids; replay suppresses duplicates while accepting a delayed id in a hole. | Implemented and tested |
| R4: A restarted client's result must come from its own incarnation. | Three-node regression puts identical old and new transactions into one slot and expects the new transaction's failure result. | Implemented and tested |
| R5: Old committed entries must still be applied; snapshot-resolved requests keep unknown-result behavior. | The incarnation filter follows application; existing pending-result and snapshot tests pass. | Implemented and tested |
| R6: Expose the API change without changing C layouts or exported functions. | Generated ABI 0.8 header, explicit exhaustion status, unsigned-id documentation, C/C++ compilation, and unchanged 27-symbol export set. | Implemented and tested |

## Validation Evidence

The following checks passed in the implementation session against the source tree now
represented by the two implementation commits. Commit preparation changed no executable
code. These are local results, recorded before the commits were published.

- `cargo +stable fmt --all --check`
- `cargo +stable clippy --workspace --all-targets --all-features --locked -- -D warnings`
- `cargo +stable nextest run --workspace --no-fail-fast --no-tests=pass -E 'not binary(model_check)'`:
  269 passed; one ignored trace-export test was skipped (it is run by `verification/tla/run.sh`).
- `cargo +stable nextest run --release -p quepaxa-sim --test model_check --no-fail-fast`:
  four passed.
- `cargo +stable test --workspace --doc`: nine passed.
- `RUSTDOCFLAGS='-D warnings' cargo +stable doc --workspace --no-deps`
- Header generation with cbindgen 0.29.4; C11 and C++17 header syntax checks.
- Release FFI build; unchanged 27 exported C symbols; C smoke test with ASan/UBSan.
- All 19 canary patches still apply. The canary executions, TLA+/TLAPS, Verus,
  Miri, and cross-platform builds were not rerun for these repairs.

Both primary regressions failed before their fixes: dedup state grew from 101 to
901 bytes after 100 writes, and the restarted transaction received the old transaction's
successful result. Both passed after the fixes.

## Delivery Checklist

- [x] Implement U1 and U2 with focused local commits.
- [x] Complete local regression and compatibility validation.
- [x] Preserve default Rust module paths for the new tests.
- [x] Complete formal review and record its scope, findings, and coverage limits.
- [x] Resolve retained findings and run affected checks if code changes (no findings or code changes).
- [x] Publish the PR description with Why, How, and Tests.
- [x] Obtain push authorization and publish the repair branch/PR.
- [ ] Verify CI on the actual published repair head.
- [ ] Obtain review approval and merge; close this record only after delivery.

## Compatibility and Review Limits

Old dedup windows are retained until normal incarnation eviction. Upgrading does not
retroactively compact old gaps. Callers must treat request ids as opaque unsigned
64-bit values; mixed reads and writes no longer return increasing ids.

## Formal Review Result

- Skill: `ce-code-review`, full review of the two implementation commits only.
- Run: `request-identity-kvw_elbo`; receipt: `status: complete`.
- Reviewed implementation head: `9b44658e295dca19440068ac40d6fa56e158fcd6`.
- Seven fresh read-only reviewer contexts: correctness, testing, maintainability,
  API contract, performance, reliability, and adversarial recovery/ordering scenarios.
- Findings: zero. Actionable findings: zero. All R1-R6 requirements and U1-U2 units met.
- Separate merge and report contexts completed the review; the validator batch was
  empty because there were no candidate findings.
- Receipt verdict: `Ready to merge`, limited to the reviewed code. Publication, CI
  on the repaired head, and merge approval remain separate gates in the checklist.

The reviewers performed static inspection and did not independently execute tests.
The runtime evidence above comes from the implementation session. Native reviewers used
separate contexts; no external cross-model service ran and no cross-provider corroboration
is claimed. The review does not certify the earlier commits in PR #132.

The raw local receipt is retained under
`/tmp/compound-engineering-1000/ce-code-review/request-identity-kvw_elbo/review.json`.
This committed summary preserves its scope, outcome, and limits if temporary artifacts
are later removed. The current publication and CI status is recorded in
[PR #132](https://github.com/dyxushuai/quepaxa/pull/132).

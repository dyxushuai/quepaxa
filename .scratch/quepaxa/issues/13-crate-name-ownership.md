# Who owns the `quepaxa` crate name on crates.io

Parent: [map](../map.md)
Type: task
Status: resolved
Blocked by:

## Question

crates.io `quepaxa` exists (v0.1.0, "A Rust library for quepaxa, a fictional project.", created 2026-07-10) and is owned by `johntaiko`, not `dyxushuai`. HITL checklist: confirm whether `johntaiko` is the user's account; if so, add `dyxushuai` as co-owner (`cargo owner --add dyxushuai quepaxa`) and record the yanking/versioning plan for the placeholder 0.1.0 (first real release must be > 0.1.0); if not, record the fallback names (`quepaxa-ffi` etc. availability) so the release-policy decision can proceed.

## Answer

`johntaiko` is the user's own crates.io account (confirmed by the user, 2026-10-13); the name `quepaxa` is ours. Consequences for release policy: the placeholder `0.1.0` ("fictional project") is already published, so the first real release must be a higher version (e.g. `0.2.0`) and the placeholder should be yanked once it lands. Optional: `cargo owner --add dyxushuai quepaxa` if publishing from the `dyxushuai` account. `quepaxa-ffi` and any other workspace crate names still need to be reserved/checked at release time.

Update (2026-10-13): the user transferred crates.io ownership of `quepaxa` to `dyxushuai` (verified via the crates.io owners API).

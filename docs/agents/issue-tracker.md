# Issue tracker: GitHub

Implementation work lives in GitHub Issues on `dyxushuai/quepaxa` (use the `gh` CLI).

- Tracking issue per release (e.g. #1 for `v0.2.0`), with each ticket as a **sub-issue** and blocking edges as **native issue dependencies** (`blocked_by`).
- Milestone per release; triage labels: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`.
- One branch and one PR per ticket; the PR body says `Closes #N`; merge only when CI (`ci-ok`) is green.
- The design decision record (wayfinder map, ADRs, spec) stays in the repo: `.scratch/quepaxa/`, `.scratch/quepaxa-v1/spec.md`, `docs/adr/`.

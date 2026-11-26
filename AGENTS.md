# Working on quepaxa

## Constraints

- The core (`crates/quepaxa`) is sans-IO, `no_std + alloc`, single-threaded and
  deterministic (ADR 0001). Clocks, randomness, network and disk come in as inputs.
- The QuePaxa paper (SOSP 2023) is the spec: read the section before designing a mechanism;
  every deviation is recorded with its reason in `docs/paper-conformance.md`.
- Architecture changes keep the design matrix in `docs/design/axiomatic-design.md`
  decoupled: a PR that adds or changes a mechanism states which FRs its DP affects and
  updates the matrix. Safety (FR1–FR4) never reads a timing parameter. A new dependency
  above the diagonal, or a DP that reads its own effect, is a loop: record it with its
  reason and tracking issue in the doc, or redesign.
- Pre-1.0: no compatibility layers. Wire, durable and C ABI formats break directly; the
  changelog says so.

## Fast iteration, correct releases

Between releases, speed wins over assurance:

- A PR needs only the fast lane: `scripts/check.sh` locally, `ci-ok` on GitHub.
- No per-change review gate, long simulations, proofs, mutants or benchmark A/Bs unless the
  change is about them.
- Merge with GitHub auto-merge: `gh pr merge <n> --auto --squash` (plain `--squash` when
  the PR is already mergeable). Branches need not be up to date with `main`.

Correctness is established at the release point:

1. Merge `chore(release): prepare X.Y.Z` (version bump, changelog dated).
2. `scripts/check.sh release X.Y.Z`: pushes `origin/main` as `release/X.Y.Z` and dispatches
   the CI heavy lane and the nightly workflow on it.
3. Fix what fails on `main`, then run step 2 again: it re-points `release/X.Y.Z`.
4. `scripts/check.sh tag X.Y.Z`: tags the commit once both runs passed on it; the release
   workflow builds the libraries. crates.io trusted publishing is not configured yet
   (#154): publish `quepaxa`, then `quepaxa-ffi`, by hand from the tag.

## Commands

```sh
scripts/check.sh            # fast lane: lint, tests (model checker included), no_std, deny
scripts/heavy <command>     # anything heavy, under one machine-wide lock
```

`scripts/heavy` matters when several sessions share a machine: wrap 100k-seed simulator
runs, `wan.sh`, benchmarks, `verification/canaries.sh` and the proofs in it, so numbers are
not measured against another session's load. The full list of checks and how to run each
one is in `VERIFICATION.md`; benchmark harnesses are under `examples/kv-tokio/bench/`.

## Conventions

- Conventional Commits, English, imperative, ≤72 characters; no attribution trailers.
- PR body: Why / How / Tests (commands and results).
- Work is tracked in GitHub issues and milestones (one per release) and the
  "quepaxa roadmap" project.
- Files stay under ~500 lines; comments only for non-obvious invariants.

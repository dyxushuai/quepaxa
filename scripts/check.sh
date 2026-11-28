#!/usr/bin/env bash
# The project's checks from one place: the fast lane CI runs on every PR (its jobs call
# this script), and the release check that runs only before a release (VERIFICATION.md).
#
#   scripts/check.sh [fast]          lint, test, no-std and deny: what a PR must pass
#   scripts/check.sh lint|test|no-std|deny
#   scripts/check.sh release X.Y.Z   push origin/main as release/X.Y.Z, dispatch CI and nightly
#   scripts/check.sh tag X.Y.Z       tag vX.Y.Z once both release-check runs passed on it
set -euo pipefail
cd "$(dirname "$0")/.."

lint() {
  cargo fmt --all --check
  cargo clippy --workspace --all-targets -- -D warnings
  # kv-tokio's shuttle build swaps std::sync for shuttle's.
  cargo clippy -p kv-tokio --all-targets --features shuttle -- -D warnings
  RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
}

tests() {
  cargo nextest run --workspace --no-fail-fast --no-tests=pass -E 'not binary(model_check) and not binary(placement) and not binary(compound_gray_faults)'
  cargo test --workspace --doc
  # The documented examples assert their results.
  for e in cluster restart membership; do cargo run -q -p quepaxa --example "$e"; done
  # Model checking (release; every scenario must be exhausted), placement, and compound gray-fault convergence.
  cargo nextest run --release -p quepaxa-sim --test model_check --test placement --test compound_gray_faults --no-fail-fast
  cargo nextest run -p kv-tokio --features shuttle --bin kv-tokio --no-fail-fast
}

no_std() { cargo build -p quepaxa --target thumbv7em-none-eabihf; }

deny() { cargo deny check; }

version_of() { git show "$1:Cargo.toml" | sed -n 's/^version = "\(.*\)"/\1/p' | head -1; }

release() {
  local v=$1 sha
  git fetch -q origin
  sha=$(git rev-parse origin/main)
  [ "$(version_of "$sha")" = "$v" ] || { echo "origin/main is not version $v" >&2; exit 1; }
  git push -q origin "$sha:refs/heads/release/$v"
  gh workflow run ci.yml --ref "release/$v"
  gh workflow run nightly.yml --ref "release/$v"
  echo "release/$v = $sha; when both runs pass: scripts/check.sh tag $v"
}

# The newest dispatched run of a workflow on release/X.Y.Z: "<conclusion> <headSha>".
run_of() {
  gh run list --workflow "$1" --branch "release/$2" --event workflow_dispatch -L 1 \
    --json conclusion,headSha --jq '.[0] | "\(.conclusion) \(.headSha)"'
}

tag() {
  local v=$1 sha w r
  git fetch -q origin
  sha=$(git rev-parse "origin/release/$v")
  for w in ci.yml nightly.yml; do
    r=$(run_of "$w" "$v")
    [ "$r" = "success $sha" ] || { echo "$w on release/$v: $r (want success $sha)" >&2; exit 1; }
  done
  git tag -a "v$v" -m "v$v" "$sha"
  git push -q origin "v$v"
  echo "tagged v$v at $sha; the release workflow builds and publishes it"
}

case ${1:-fast} in
  fast) lint; tests; no_std; deny ;;
  lint) lint ;;
  test) tests ;;
  no-std) no_std ;;
  deny) deny ;;
  release | tag) [ $# -eq 2 ] || { echo "usage: $0 $1 X.Y.Z" >&2; exit 2; }; "$1" "$2" ;;
  *) sed -n '2,8p' "$0" >&2; exit 2 ;;
esac

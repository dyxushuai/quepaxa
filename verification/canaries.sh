#!/usr/bin/env bash
# Seeded-bug canaries: every patch in verification/canaries/ plants a known bug; the
# listed check must fail with the expected message. A canary that applies but is not
# caught means the verification got weaker; one that no longer applies means the code
# moved and the canary must be refreshed. Either way this script fails.
#
# Usage: verification/canaries.sh [name-filter]
set -uo pipefail
root=$(git rev-parse --show-toplevel)
filter=${1:-}
work=$(mktemp -d)
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$root/target/canaries}
failed=0
ran=0
# Canaries run against a snapshot of the working tree (new files included, ignored files
# not), so local edits are checked too.
index=$(mktemp)
cp "$(git -C "$root" rev-parse --path-format=absolute --git-path index)" "$index"
GIT_INDEX_FILE=$index git -C "$root" add -A
rev=$(GIT_AUTHOR_NAME=canaries GIT_AUTHOR_EMAIL=canaries@localhost GIT_COMMITTER_NAME=canaries \
  GIT_COMMITTER_EMAIL=canaries@localhost \
  git -C "$root" commit-tree "$(GIT_INDEX_FILE=$index git -C "$root" write-tree)" -p HEAD -m canaries)
rm -f "$index"
[[ -n "$rev" ]] || { echo "cannot snapshot the working tree"; exit 1; }
for patch in "$root"/verification/canaries/*.patch; do
  name=$(basename "$patch" .patch)
  if ! cut -f1 "$root/verification/canaries.tsv" | grep -qx "$name"; then
    echo "UNLISTED $name: add it to verification/canaries.tsv"
    failed=1
  fi
done
while IFS=$'\t' read -r name env cmd expect; do
  [[ -z "$name" || "$name" == \#* ]] && continue
  [[ -n "$filter" && "$name" != *"$filter"* ]] && continue
  ran=$((ran + 1))
  tree="$work/$name"
  git -C "$root" worktree add -q --detach "$tree" "$rev"
  if ! git -C "$tree" apply "$root/verification/canaries/$name.patch"; then
    echo "STALE   $name: patch no longer applies; refresh it"
    failed=1
  else
    envs=(); [[ "$env" != "-" ]] && read -r -a envs <<< "$env"
    out=$(cd "$tree" && env "${envs[@]}" bash -c "$cmd" 2>&1); status=$?
    if [[ $status -ne 0 ]] && grep -qE "$expect" <<< "$out"; then
      echo "CAUGHT  $name: $(grep -m1 -oE "$expect" <<< "$out")"
    elif [[ $status -eq 0 ]]; then
      echo "MISSED  $name: '$cmd' passed with the bug planted"
      failed=1
    else
      echo "WRONG   $name: '$cmd' failed without /$expect/:"; tail -5 <<< "$out"
      failed=1
    fi
  fi
  git -C "$root" worktree remove --force "$tree"
done < "$root/verification/canaries.tsv"
rm -rf "$work"
if [[ $ran -eq 0 ]]; then
  echo "no canary matches '$filter'"
  failed=1
fi
exit $failed

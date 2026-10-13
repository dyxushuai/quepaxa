#!/usr/bin/env bash
# Fails if cargo-mutants missed a mutant that verification/mutants-accepted.txt does not
# list, so mutation coverage can only ratchet up. Locations are ignored (they move with
# every edit); a mutant is its file, operator and function, and the list must name it as
# many times as it is missed (one entry per site).
#
# Usage: verification/mutants-check.sh mutants.out/missed.txt
set -euo pipefail
root=$(git rev-parse --show-toplevel)
missed=${1:?usage: $0 mutants.out/missed.txt}
[[ -f "$missed" ]] || { echo "no missed mutants"; exit 0; }
normalize() { sed -E 's/:[0-9]+:[0-9]+: /: /'; }
new=$(awk 'NR == FNR { allowed[$0]++; next } ++seen[$0] > allowed[$0]' \
  <(grep -v -e '^#' -e '^$' "$root/verification/mutants-accepted.txt" | normalize) <(normalize < "$missed"))
if [[ -n "$new" ]]; then
  echo "Missed mutants not in verification/mutants-accepted.txt (add a test, or accept with a reason):"
  echo "$new"
  exit 1
fi
echo "no new missed mutants"

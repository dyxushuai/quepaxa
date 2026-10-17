#!/usr/bin/env bash
# Trace validation: crates/quepaxa/src/slot_sim.rs runs the real Register and SlotProposer
# over a faulty network and logs every delivered message with the state after it; TLC
# (QuePaxaTrace.tla) checks that each log is a behaviour of QuePaxa.tla. Prints REJECTED
# and fails if any is not, which means the implementation did something the spec does not
# allow (or the spec is missing a behaviour).
#
# Needs Java 11+ (or Docker) and cargo. Usage: verification/tla/traces.sh
# QPX_TLA_TRACES traces (default 30), of 3 or 5 recorders and 2 or 3 proposers.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
source "$here/lib.sh"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir "$work/traces" "$work/spec"
(cd "$root" && QPX_TLA_TRACE_DIR="$work/traces" QPX_TLA_TRACES="${QPX_TLA_TRACES:-30}" cargo test -q -p quepaxa --lib export_tla_traces -- --ignored) \
  > "$work/export.log" 2>&1 || { echo "FAIL    exporting traces:"; tail -20 "$work/export.log"; exit 1; }
cp "$here/QuePaxa.tla" "$here/QuePaxaTrace.tla" "$here/QuePaxaTrace.cfg" "$work/spec/"

failed=0 accepted=0
for trace in "$work"/traces/trace-*.tla; do
  cp "$trace" "$work/spec/TraceData.tla"
  entries=$(grep -c "event |->" "$trace")
  out=$(tlc "$work/spec" 600 QuePaxaTrace 2>&1)
  status=$?
  if [[ $status -eq 12 ]] && grep -q "Invariant Unfinished is violated" <<< "$out"; then
    accepted=$((accepted + 1))
  else
    matched=$(grep -oE "[0-9,]+ distinct states found" <<< "$out" | tail -1 | grep -oE "^[0-9,]+")
    echo "REJECTED $(basename "$trace" .tla): matched ${matched:-0} of $entries entries (TLC status $status)"
    grep -E "^Error" <<< "$out" | head -5
    failed=1
  fi
done
[[ $failed -eq 0 ]] && echo "OK      trace validation: $accepted traces of slot_sim are behaviours of QuePaxa.tla"
exit $failed

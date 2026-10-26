#!/usr/bin/env bash
# Checks the independent TLA+ models with TLC:
# - QuePaxa.tla, one slot: too large to exhaust, so TLC runs random behaviours (simulation
#   mode) for TLA_SECONDS and must find no violation; so must QuePaxaProof.tla, the proof's
#   inductive invariant on the same model (prove.sh checks the proof itself).
# - Membership.tla, changes across slots, durability and retirement: checked exhaustively.
# Then every seeded bug must be found (within TLA_BUG_SECONDS), so the check cannot
# silently stop checking anything.
#
# Finally, trace validation (traces.sh) checks the Rust implementation against
# QuePaxa.tla.
#
# Needs Java 11+ (or Docker) and cargo. Usage: verification/tla/run.sh
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
seconds=${TLA_SECONDS:-120}
bug_seconds=${TLA_BUG_SECONDS:-120}
membership_seconds=${TLA_MEMBERSHIP_SECONDS:-1800}
source "$here/lib.sh"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failed=0

simulate=(-simulate -depth 300)

# simulate <label> <spec> <files...>: random behaviours for $seconds, no violation allowed.
simulate_ok() {
  local label=$1 spec=$2 dir=$work/$2
  mkdir "$dir"
  cp "${@:3}" "$dir/"
  local out status
  out=$(tlc "$dir" "$seconds" "$spec" -config "$spec.cfg" "${simulate[@]}" 2>&1)
  status=$?
  if [[ $status -ne 124 ]] || ! grep -q "Running Random Simulation" <<< "$out"; then
    echo "FAIL    $label: TLC ended with status $status instead of running out of time:"; tail -40 <<< "$out"
    failed=1
  elif grep -qE "violated|^Error" <<< "$out"; then
    echo "FAIL    $label:"; grep -E -A30 "violated|^Error" <<< "$out" | head -60
    failed=1
  else
    echo "OK      $label: no violation in ${seconds}s ($(grep -oE '[0-9,]+ states checked' <<< "$out" | tail -1 || echo 'progress is printed every minute'))"
  fi
}

simulate_ok QuePaxa.tla QuePaxa "$here/QuePaxa.tla" "$here/QuePaxa.cfg"
# The proof's inductive invariant (prove.sh) on reachable states: a change to QuePaxa.tla
# that breaks it shows up here in minutes.
simulate_ok "QuePaxaProof.tla (IndInv)" QuePaxaProof \
  "$here/QuePaxa.tla" "$here/QuePaxaProof.tla" "$here/QuePaxaProof.cfg"

for cfg in Membership MembershipQuorum; do
  mkdir "$work/$cfg"
  cp "$here/Membership.tla" "$here/$cfg.cfg" "$work/$cfg/"
  out=$(tlc "$work/$cfg" "$membership_seconds" Membership -config "$cfg.cfg" 2>&1)
  status=$?
  if [[ $status -ne 0 ]] || ! grep -q "0 states left on queue" <<< "$out"; then
    echo "FAIL    Membership.tla ($cfg.cfg): TLC ended with status $status:"
    grep -E -A30 "violated|^Error" <<< "$out" | head -60; tail -5 <<< "$out"
    failed=1
  else
    echo "OK      Membership.tla ($cfg.cfg): exhaustive, $(grep -oE '[0-9,]+ distinct states found' <<< "$out" | tail -1)"
  fi
done

# spec, its configuration, name, invariant it must violate, then a sed expression
# planting the bug.
bugs=(
  QuePaxa QuePaxa "decide-on-any-aggregate" Agreement 's/IF best = s\.p$/IF best # Nil/'
  QuePaxa QuePaxa "skip-phase3-adoption" Agreement 's/next\(IF best = Nil THEN s\.p ELSE best\)/next(s.p)/'
  Membership Membership "window-one-slot-too-wide" ViewAgree 's/    \/\\ s < Prefix\(log\[r\]\) \+ LAG$/    \/\\ s < Prefix(log[r]) + LAG + 1/'
  Membership Membership "change-takes-effect-early" ViewAgree 's/!\.from = t \+ LAG\]/!.from = t + LAG - 1]/'
  Membership Membership "retire-on-own-progress" Recoverable 's/       \/\\ 2 \* Cardinality\(caught\) > Cardinality\(st\.nxt\)/       \/\\ Prefix(log[r]) >= st.from/'
  Membership MembershipQuorum "retire-on-one-durable-copy" Recoverable 's/       \/\\ 2 \* Cardinality\(caught\) > Cardinality\(st\.nxt\)/       \/\\ Cardinality(caught) > 0/'
  Membership Membership "retire-on-progress-in-memory" Recoverable 's/caught == \{m \\in st\.nxt : Prefix\(dlog\[m\]\) >= st\.from\}/caught == {m \\in st.nxt : Prefix(log[m]) >= st.from}/'
)
for ((i = 0; i < ${#bugs[@]}; i += 5)); do
  spec=${bugs[i]} cfg=${bugs[i + 1]} name=${bugs[i + 2]} invariant=${bugs[i + 3]}
  mode=(-config "$cfg.cfg")
  [[ $spec == QuePaxa ]] && mode+=("${simulate[@]}")
  mkdir "$work/$name"
  sed -E "${bugs[i + 4]}" "$here/$spec.tla" > "$work/$name/$spec.tla"
  cp "$here/$cfg.cfg" "$work/$name/"
  if cmp -s "$here/$spec.tla" "$work/$name/$spec.tla"; then
    echo "STALE   $name: the sed expression no longer matches $spec.tla"
    failed=1
  elif tlc "$work/$name" "$bug_seconds" "$spec" "${mode[@]}" > "$work/$name.log" 2>&1; [[ $? -eq 12 ]] && grep -q "Invariant $invariant is violated" "$work/$name.log"; then
    echo "CAUGHT  $name"
  else
    echo "MISSED  $name: no $invariant violation within ${bug_seconds}s"; tail -5 "$work/$name.log"
    failed=1
  fi
done
"$here/traces.sh" || failed=1
exit $failed

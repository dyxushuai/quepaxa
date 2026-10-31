#!/usr/bin/env bash
# Checks the independent TLA+ models with TLC:
# - QuePaxa.tla, one slot: too large to exhaust, so TLC runs random behaviours (simulation
#   mode) for TLA_SECONDS and must find no violation; so must QuePaxaProof.tla, the proof's
#   inductive invariant on the same model (prove.sh checks the proof itself).
# - Membership.tla, changes across slots, durability and retirement: checked exhaustively.
# - Reads.tla, linearizable reads on top of it: exhaustive with fixed membership, simulated
#   across membership changes (too large to exhaust).
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

# simulate <label> <spec> <config> <files...>: random behaviours for $seconds, no violation
# allowed.
simulate_ok() {
  local label=$1 spec=$2 cfg=$3 dir=$work/$3
  mkdir "$dir"
  cp "${@:4}" "$dir/"
  local out status
  out=$(tlc "$dir" "$seconds" "$spec" -config "$cfg.cfg" "${simulate[@]}" 2>&1)
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

simulate_ok QuePaxa.tla QuePaxa QuePaxa "$here/QuePaxa.tla" "$here/QuePaxa.cfg"
# The proof's inductive invariant (prove.sh) on reachable states: a change to QuePaxa.tla
# that breaks it shows up here in minutes.
simulate_ok "QuePaxaProof.tla (IndInv)" QuePaxaProof QuePaxaProof \
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

mkdir "$work/ReadsFixed"
cp "$here/Membership.tla" "$here/Reads.tla" "$here/ReadsFixed.cfg" "$work/ReadsFixed/"
out=$(tlc "$work/ReadsFixed" "$membership_seconds" Reads -config ReadsFixed.cfg 2>&1)
status=$?
if [[ $status -ne 0 ]] || ! grep -q "0 states left on queue" <<< "$out"; then
  echo "FAIL    Reads.tla (ReadsFixed.cfg): TLC ended with status $status:"
  grep -E -A30 "violated|^Error" <<< "$out" | head -60; tail -5 <<< "$out"
  failed=1
else
  echo "OK      Reads.tla (ReadsFixed.cfg): exhaustive, $(grep -oE '[0-9,]+ distinct states found' <<< "$out" | tail -1)"
fi
for cfg in Reads ReadsTwoChanges; do
  simulate_ok "Reads.tla ($cfg.cfg)" Reads "$cfg" "$here/Membership.tla" "$here/Reads.tla" "$here/$cfg.cfg"
done

# spec, its configuration, name, invariant it must violate, seconds to find it ("-" for
# TLA_BUG_SECONDS), then a sed expression planting the bug. Configurations too large to
# exhaust are simulated.
covering='    LET st == Latest\(r\) IN \{MembersAt\(t, PrefixKnown\(r\)\) : t \\in k\.\.N-1\} \\cup \{st\.cur, st\.nxt\}'
bugs=(
  QuePaxa QuePaxa "decide-on-any-aggregate" Agreement - 's/IF best = s\.p$/IF best # Nil/'
  QuePaxa QuePaxa "skip-phase3-adoption" Agreement - 's/next\(IF best = Nil THEN s\.p ELSE best\)/next(s.p)/'
  Membership Membership "window-one-slot-too-wide" ViewAgree - 's/    \/\\ s < Prefix\(log\[r\]\) \+ LAG$/    \/\\ s < Prefix(log[r]) + LAG + 1/'
  Membership Membership "change-takes-effect-early" ViewAgree - 's/!\.from = t \+ LAG\]/!.from = t + LAG - 1]/'
  Membership Membership "retire-on-own-progress" RecoverableFast - 's/       \/\\ 2 \* Cardinality\(caught\) > Cardinality\(st\.nxt\)/       \/\\ Prefix(log[r]) >= st.from/'
  Membership MembershipQuorum "retire-on-one-durable-copy" RecoverableFast - 's/       \/\\ 2 \* Cardinality\(caught\) > Cardinality\(st\.nxt\)/       \/\\ Cardinality(caught) > 0/'
  Membership Membership "retire-on-progress-in-memory" RecoverableFast - 's/caught == \{m \\in st\.nxt : Prefix\(dlog\[m\]\) >= st\.from\}/caught == {m \\in st.nxt : Prefix(log[m]) >= st.from}/'
  Reads ReadsFixed "read-mark-from-decided-slots-only" ReadSafe - 's/LET held == (vreg\[r\] \\cup )?reg\[r\] \\cup \{t/LET held == {t/'
  Reads ReadsFixed "read-mark-from-registers-only" ReadSafe - 's/LET held == ((vreg\[r\] \\cup )?reg\[r\]) \\cup \{t \\in d?log\[r\] : t >= floor\[r\]\}/LET held == \1/'
  Reads ReadsFixed "read-mark-without-floor" ReadSafe - 's/    IN Max\(\{floor\[r\]\} \\cup \{t \+ 1 : t \\in held\}\)$/    IN Max({0} \\cup {t + 1 : t \\in held})/'
  Reads ReadsFixed "read-on-a-minority" ReadSafe - 's/2 \* Cardinality\(A \\cap E\) > Cardinality\(E\)/Cardinality(A \\cap E) > 0/'
  Reads ReadsFixed "read-before-applying-k" ReadSafe - 's/       \/\\ Prefix\(log\[r\]\) >= K$/       \/\\ TRUE/'
  Reads ReadsFixed "read-counts-earlier-answers" ReadSafe - 's/before \|-> \{t \\in Slots : decided\[t\] # None\}, who \|-> \{\}, k \|-> 1,/before |-> {t \\in Slots : decided[t] # None}, who |-> rd[i].who, k |-> rd[i].k,/; s/^    \/\\ rd\[i\]\.st = "open"$/    \/\\ rd[i].st \\in {"idle", "open"}/'
  Reads ReadsTwoChanges "read-asks-the-newest-epoch-only" ReadSafe - "s/$covering/    LET st == Latest(r) IN {st.nxt}/"
)
for ((i = 0; i < ${#bugs[@]}; i += 6)); do
  spec=${bugs[i]} cfg=${bugs[i + 1]} name=${bugs[i + 2]} invariant=${bugs[i + 3]} limit=${bugs[i + 4]}
  [[ $limit == - ]] && limit=$bug_seconds
  mode=(-config "$cfg.cfg")
  [[ $spec == QuePaxa || $cfg == Reads || $cfg == ReadsTwoChanges ]] && mode+=("${simulate[@]}")
  mkdir "$work/$name"
  sed -E "${bugs[i + 5]}" "$here/$spec.tla" > "$work/$name/$spec.tla"
  cp "$here/$cfg.cfg" "$work/$name/"
  [[ $spec == Reads ]] && cp "$here/Membership.tla" "$work/$name/"
  if cmp -s "$here/$spec.tla" "$work/$name/$spec.tla"; then
    echo "STALE   $name: the sed expression no longer matches $spec.tla"
    failed=1
  elif tlc "$work/$name" "$limit" "$spec" "${mode[@]}" > "$work/$name.log" 2>&1; [[ $? -eq 12 ]] && grep -q "Invariant $invariant is violated" "$work/$name.log"; then
    echo "CAUGHT  $name"
  else
    echo "MISSED  $name: no $invariant violation within ${limit}s"; tail -5 "$work/$name.log"
    failed=1
  fi
done
"$here/traces.sh" || failed=1
exit $failed

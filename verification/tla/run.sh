#!/usr/bin/env bash
# Checks the independent TLA+ models with TLC:
# - QuePaxa.tla, one slot: too large to exhaust, so TLC runs random behaviours (simulation
#   mode) for TLA_SECONDS and must find no violation.
# - Membership.tla, changes across slots, durability and retirement: checked exhaustively.
# Then every seeded bug must be found (within TLA_BUG_SECONDS), so the check cannot
# silently stop checking anything.
#
# Needs Java 11+ (or Docker). Usage: verification/tla/run.sh
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
seconds=${TLA_SECONDS:-120}
bug_seconds=${TLA_BUG_SECONDS:-120}
membership_seconds=${TLA_MEMBERSHIP_SECONDS:-1800}
version=v1.7.4
sha256=936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88
tools=$root/target/tla
jar=$tools/tla2tools-$version.jar

mkdir -p "$tools"
if [[ ! -f "$jar" ]]; then
  curl -fsSL -o "$jar.part" "https://github.com/tlaplus/tlaplus/releases/download/$version/tla2tools.jar" || exit 1
  echo "$sha256  $jar.part" | sha256sum -c --quiet - || { rm -f "$jar.part"; exit 1; }
  mv "$jar.part" "$jar"
fi

# tlc <spec dir> <seconds> <spec> [mode args]: prints TLC's output and returns its status:
# 0 when an exhaustive check finished, 124 when the time ran out (no violation found in
# simulation), 12 for an invariant violation; anything else is a failure.
tlc() {
  local args=(-cp tla2tools.jar tlc2.TLC "${@:4}" -workers auto -deadlock -metadir /tmp/tlc-$$ "$3.tla")
  cp "$jar" "$1/tla2tools.jar"
  local status
  if command -v java >/dev/null; then
    (cd "$1" && timeout "$2" java -XX:+UseParallelGC "${args[@]}")
    status=$?
  else
    # The time limit runs inside the container: killing the client would leave it running.
    docker run --rm -v "$1:/spec" -w /spec eclipse-temurin:21.0.4_7-jre timeout "$2" java "${args[@]}"
    status=$?
  fi
  rm -f "$1/tla2tools.jar"
  return $status
}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failed=0

simulate=(-simulate -depth 300)

mkdir "$work/model"
cp "$here/QuePaxa.tla" "$here/QuePaxa.cfg" "$work/model/"
out=$(tlc "$work/model" "$seconds" QuePaxa "${simulate[@]}" 2>&1)
status=$?
if [[ $status -ne 124 ]] || ! grep -q "Running Random Simulation" <<< "$out"; then
  echo "FAIL    QuePaxa.tla: TLC ended with status $status instead of running out of time:"; tail -40 <<< "$out"
  failed=1
elif grep -qE "violated|^Error" <<< "$out"; then
  echo "FAIL    QuePaxa.tla:"; grep -E -A30 "violated|^Error" <<< "$out" | head -60
  failed=1
else
  echo "OK      QuePaxa.tla: no violation in ${seconds}s ($(grep -oE '[0-9,]+ states checked' <<< "$out" | tail -1 || echo 'progress is printed every minute'))"
fi

mkdir "$work/membership"
cp "$here/Membership.tla" "$here/Membership.cfg" "$work/membership/"
out=$(tlc "$work/membership" "$membership_seconds" Membership 2>&1)
status=$?
if [[ $status -ne 0 ]] || ! grep -q "0 states left on queue" <<< "$out"; then
  echo "FAIL    Membership.tla: TLC ended with status $status:"; grep -E -A30 "violated|^Error" <<< "$out" | head -60; tail -5 <<< "$out"
  failed=1
else
  echo "OK      Membership.tla: exhaustive, $(grep -oE '[0-9,]+ distinct states found' <<< "$out" | tail -1)"
fi

# spec, name, invariant it must violate, then a sed expression planting the bug.
bugs=(
  QuePaxa "decide-on-any-aggregate" Agreement 's/IF best = s\.p$/IF best # Nil/'
  QuePaxa "skip-phase3-adoption" Agreement 's/next\(IF best = Nil THEN s\.p ELSE best\)/next(s.p)/'
  Membership "window-one-slot-too-wide" ViewAgree 's/    \/\\ s < Prefix\(log\[r\]\) \+ LAG$/    \/\\ s < Prefix(log[r]) + LAG + 1/'
  Membership "change-takes-effect-early" ViewAgree 's/!\.from = t \+ LAG\]/!.from = t + LAG - 1]/'
  Membership "retire-on-own-progress" Recoverable 's/       \/\\ 2 \* Cardinality\(caught\) > Cardinality\(st\.nxt\)/       \/\\ Prefix(log[r]) >= st.from/'
  Membership "retire-on-one-durable-copy" Recoverable 's/       \/\\ 2 \* Cardinality\(caught\) > Cardinality\(st\.nxt\)/       \/\\ Cardinality(caught) > 0/'
  Membership "retire-on-progress-in-memory" Recoverable 's/caught == \{m \\in st\.nxt : Prefix\(dlog\[m\]\) >= st\.from\}/caught == {m \\in st.nxt : Prefix(log[m]) >= st.from}/'
)
for ((i = 0; i < ${#bugs[@]}; i += 4)); do
  spec=${bugs[i]} name=${bugs[i + 1]} invariant=${bugs[i + 2]}
  mode=()
  [[ $spec == QuePaxa ]] && mode=("${simulate[@]}")
  mkdir "$work/$name"
  sed -E "${bugs[i + 3]}" "$here/$spec.tla" > "$work/$name/$spec.tla"
  cp "$here/$spec.cfg" "$work/$name/"
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
exit $failed

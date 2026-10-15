#!/usr/bin/env bash
# Checks the independent TLA+ model of one QuePaxa slot (QuePaxa.tla) with TLC.
#
# The model is too large to exhaust, so TLC runs random behaviours (simulation mode) for
# TLA_SECONDS and must find no violation. Then two seeded protocol bugs must each be found
# within TLA_BUG_SECONDS, so the check cannot silently stop checking anything.
#
# Needs Java 11+ (or Docker). Usage: verification/tla/run.sh
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
seconds=${TLA_SECONDS:-120}
bug_seconds=${TLA_BUG_SECONDS:-120}
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

# tlc <spec dir> <seconds>: prints TLC's output and returns its status: 124 when the time
# ran out (no violation found), 12 for an invariant violation, anything else is a failure.
tlc() {
  local args=(-cp tla2tools.jar tlc2.TLC -simulate -depth 300 -workers auto -deadlock -metadir /tmp/tlc-$$ QuePaxa.tla)
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

mkdir "$work/model"
cp "$here/QuePaxa.tla" "$here/QuePaxa.cfg" "$work/model/"
out=$(tlc "$work/model" "$seconds" 2>&1)
status=$?
if [[ $status -ne 124 ]] || ! grep -q "Running Random Simulation" <<< "$out"; then
  echo "FAIL    TLC ended with status $status instead of running out of time:"; tail -40 <<< "$out"
  failed=1
elif grep -qE "violated|^Error" <<< "$out"; then
  echo "FAIL    QuePaxa.tla:"; grep -E -A30 "violated|^Error" <<< "$out" | head -60
  failed=1
else
  echo "OK      QuePaxa.tla: no violation in ${seconds}s ($(grep -oE '[0-9,]+ states checked' <<< "$out" | tail -1 || echo 'progress is printed every minute'))"
fi

# name, then a sed expression planting the bug.
bugs=(
  "decide-on-any-aggregate" 's/IF best = s\.p$/IF best # Nil/'
  "skip-phase3-adoption" 's/next\(IF best = Nil THEN s\.p ELSE best\)/next(s.p)/'
)
for ((i = 0; i < ${#bugs[@]}; i += 2)); do
  name=${bugs[i]}
  mkdir "$work/$name"
  sed -E "${bugs[i + 1]}" "$here/QuePaxa.tla" > "$work/$name/QuePaxa.tla"
  cp "$here/QuePaxa.cfg" "$work/$name/"
  if cmp -s "$here/QuePaxa.tla" "$work/$name/QuePaxa.tla"; then
    echo "STALE   $name: the sed expression no longer matches QuePaxa.tla"
    failed=1
  elif tlc "$work/$name" "$bug_seconds" > "$work/$name.log" 2>&1; [[ $? -eq 12 ]] && grep -q "Invariant Agreement is violated" "$work/$name.log"; then
    echo "CAUGHT  $name"
  else
    echo "MISSED  $name: no Agreement violation within ${bug_seconds}s"; tail -5 "$work/$name.log"
    failed=1
  fi
done
exit $failed

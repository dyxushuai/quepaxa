# Shared by run.sh and traces.sh: fetches TLC (pinned, checksum verified) and runs it.
# Expects $root (the repository) to be set.
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
  # A fresh state directory per run: TLC names its subdirectory by the time to the second,
  # so back-to-back runs sharing one collided and the second failed to start.
  local meta
  meta=$(mktemp -d)
  local args=(-cp tla2tools.jar tlc2.TLC "${@:4}" -workers auto -deadlock -metadir "$meta" "$3.tla")
  cp "$jar" "$1/tla2tools.jar"
  local status
  if command -v java >/dev/null; then
    (cd "$1" && timeout "$2" java -XX:+UseParallelGC "${args[@]}")
    status=$?
  else
    args=(-cp tla2tools.jar tlc2.TLC "${@:4}" -workers auto -deadlock -metadir /tmp/tlc "$3.tla")
    # The time limit runs inside the container: killing the client would leave it running.
    docker run --rm -v "$1:/spec" -w /spec eclipse-temurin:21.0.4_7-jre timeout "$2" java "${args[@]}"
    status=$?
  fi
  rm -f "$1/tla2tools.jar"
  rm -rf "${meta:?}"
  return $status
}

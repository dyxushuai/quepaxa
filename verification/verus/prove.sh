#!/usr/bin/env bash
# Checks the Verus proofs on ports of the replica's code, then plants known bugs that the
# proofs must reject: the durability gate (gate.rs, from crates/quepaxa/src/replica/gate.rs)
# never lets a send or effect overtake the writes it depends on; commit-time deduplication
# (dedup.rs, from crates/quepaxa/src/dedup.rs) applies an entry at most once. Uses Verus, fetched
# (pinned, checksum verified) into target/verus-<digest>, and the Rust toolchain that
# release requires (installed with rustup if missing). Linux x86_64 only.
#
# Usage: verification/verus/prove.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
version=0.2026.10.04.426d8b0
sha256=624018662868563a7bec0dd07be5e4dd0c3d61124b4b55105765884bf2d4d1a0
toolchain=1.98.1-x86_64-unknown-linux-gnu
url=https://github.com/verus-lang/verus/releases/download/release/$version/verus-$version-x86-linux.zip
# The digest in the path: a new pin never reuses an install of another archive.
tools=$root/target/verus-${sha256:0:12}
work=$root/target/verus-work

if [[ ! -x $tools/verus ]]; then
  mkdir -p "$root/target"
  curl -fsSL -o "$tools.zip.part" "$url"
  [[ $(sha256sum "$tools.zip.part" | cut -d' ' -f1) == "$sha256" ]] || {
    echo "Verus archive checksum mismatch"; rm -f "$tools.zip.part"; exit 1; }
  rm -rf "$tools.unpack" && mkdir "$tools.unpack"
  unzip -q "$tools.zip.part" -d "$tools.unpack"
  rm -rf "$tools" && mv "$tools.unpack/verus-x86-linux" "$tools"
  rm -rf "$tools.unpack" "$tools.zip.part"
fi
rustup run "$toolchain" rustc --version >/dev/null 2>&1 ||
  rustup toolchain install "$toolchain" --profile minimal

verus() { "$tools/verus" --crate-type=lib "$@"; }

verus "$here/gate.rs"
verus "$here/dedup.rs"

# Each bug: the file, a name, then a sed expression that plants it. The proof must fail on
# every one.
bugs=(
  gate  reply-before-own-writes-durable  '0,/if self\.idle\(\)/s//if self.ready()/'
  gate  park-behind-oldest-persist       's/self\.in_flight\[n - 1\]\./self.in_flight[0]./g'
  gate  release-one-persist-too-many     's/self\.in_flight\[0\]\.id > id \{/self.in_flight[0].id > id + 1 {/'
  gate  persist-drops-its-records        's/actions\.push\(Action::Persist \{ id, records,/actions.push(Action::Persist { id, records: Vec::new(),/'
  gate  checkpoint-appends               's/        self\.replace = true;/        self.replace = false;/'
  dedup evicted-incarnation-reapplied    's/            if min_key_gt\(incs, incarnation\) \{/            if false \&\& min_key_gt(incs, incarnation) {/'
  dedup id-below-window-reapplied        's/let fresh = id >= w\.below \&\& /let fresh = /'
  dedup window-skips-an-id               's/w\.below = w\.below \+ 1;/w.below = w.below + 2;/'
  dedup contains-forgets-evictions       's/None => incs\.len\(\) >= INCARNATIONS_KEPT \&\& min_key_gt\(incs, incarnation\),/None => false,/'
)
mkdir -p "$work"
status=0
for ((i = 0; i < ${#bugs[@]}; i += 3)); do
  file=${bugs[i]} name=${bugs[i + 1]}
  sed -E "${bugs[i + 2]}" "$here/$file.rs" > "$work/$name.rs"
  if cmp -s "$here/$file.rs" "$work/$name.rs"; then
    echo "STALE   $name (the bug no longer applies: refresh it against gate.rs)"; status=1
  elif verus "$work/$name.rs" > "$work/$name.log" 2>&1; then
    echo "MISSED  $name"; status=1
  elif grep -q "verification results:: .* [1-9][0-9]* errors" "$work/$name.log"; then
    echo "CAUGHT  $name"
  else
    echo "BROKEN  $name (did not reach verification: see $work/$name.log)"; status=1
  fi
done
exit $status

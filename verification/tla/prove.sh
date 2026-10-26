#!/usr/bin/env bash
# Checks the TLAPS proofs: QuePaxa.tla is safe (QuePaxaSafety.tla, QuePaxaRecordStep.tla,
# QuePaxaInductive.tla), and so are Membership.tla's window rule and retirement
# (MembershipProof.tla, MembershipRecover.tla). Uses tlapm 1.6, fetched (pinned, checksum
# verified) into target/tlapm-<digest>. On Linux x86_64 it runs directly; elsewhere, in a
# linux/amd64 ubuntu:24.04 container (needs Docker). Arguments go to tlapm, e.g. --cleanfp
# to re-prove every obligation instead of reusing proved ones.
#
# tlapm 1.6 is only published as the rolling `1.6.0-pre` release (the only one that bundles
# its provers): if upstream replaces the archive, the checksum fails loudly; pin the new
# archive's digest after re-running the proof with it.
#
# Usage: verification/tla/prove.sh [tlapm args]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
version=1.6.0-pre
sha256=0d1b393a1042aa08961bb024c784a80a25009a5874258c0ddbbfd95b92e80fe5
url=https://github.com/tlaplus/tlapm/releases/download/$version/tlapm-$version-x86_64-linux-gnu.tar.gz
# The digest in the path: a new pin never reuses an install of another archive.
tools=$root/target/tlapm-${sha256:0:12}
work=$root/target/tlaps

digest() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi; }
if [[ ! -x $tools/bin/tlapm ]]; then
  mkdir -p "$root/target"
  curl -fsSL -o "$tools.tar.gz.part" "$url"
  [[ $(digest "$tools.tar.gz.part" | cut -d' ' -f1) == "$sha256" ]] || {
    echo "tlapm archive checksum mismatch (upstream replaced $version?)"; rm -f "$tools.tar.gz.part"; exit 1; }
  rm -rf "$tools.unpack" && mkdir "$tools.unpack"
  tar xzf "$tools.tar.gz.part" -C "$tools.unpack"
  rm -rf "$tools" && mv "$tools.unpack/tlapm" "$tools"
  rm -rf "$tools.unpack" "$tools.tar.gz.part"
fi

mkdir -p "$work"
modules=(QuePaxaSafety.tla QuePaxaRecordStep.tla QuePaxaInductive.tla MembershipProof.tla
  MembershipRecover.tla)
cp "$here"/QuePaxa.tla "$here"/QuePaxaProof.tla "$here"/Membership.tla "${modules[@]/#/$here/}" "$work/"
if [[ $(uname -sm) == "Linux x86_64" ]]; then
  cd "$work" && exec "$tools/bin/tlapm" "$@" "${modules[@]}"
fi
exec docker run --rm --platform linux/amd64 --user "$(id -u):$(id -g)" \
  -v "$tools:/tlapm:ro" -v "$work:/w" -w /w ubuntu:24.04 /tlapm/bin/tlapm "$@" "${modules[@]}"

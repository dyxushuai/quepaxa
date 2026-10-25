#!/usr/bin/env bash
# Checks the TLAPS proof that QuePaxa.tla is safe (QuePaxaSafety.tla, QuePaxaRecordStep.tla,
# QuePaxaInductive.tla) with tlapm 1.6, fetched (pinned, checksum verified) into
# target/tlapm. On Linux x86_64 it runs directly; elsewhere, in an ubuntu:24.04 container
# (needs Docker). Arguments go to tlapm, e.g. --cleanfp to re-prove every obligation
# instead of reusing proved ones.
#
# Usage: verification/tla/prove.sh [tlapm args]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
version=1.6.0-pre
sha256=0d1b393a1042aa08961bb024c784a80a25009a5874258c0ddbbfd95b92e80fe5
url=https://github.com/tlaplus/tlapm/releases/download/$version/tlapm-$version-x86_64-linux-gnu.tar.gz
tools=$root/target/tlapm
work=$root/target/tlaps

if [[ ! -x $tools/bin/tlapm ]]; then
  mkdir -p "$root/target"
  curl -fsSL -o "$tools.tar.gz.part" "$url"
  echo "$sha256  $tools.tar.gz.part" | sha256sum -c --quiet - || { rm -f "$tools.tar.gz.part"; exit 1; }
  rm -rf "$tools.unpack" && mkdir "$tools.unpack"
  tar xzf "$tools.tar.gz.part" -C "$tools.unpack"
  rm -rf "$tools" && mv "$tools.unpack/tlapm" "$tools"
  rm -rf "$tools.unpack" "$tools.tar.gz.part"
fi

mkdir -p "$work"
modules=(QuePaxaSafety.tla QuePaxaRecordStep.tla QuePaxaInductive.tla)
cp "$here"/QuePaxa.tla "$here"/QuePaxaProof.tla "${modules[@]/#/$here/}" "$work/"
if [[ $(uname -sm) == "Linux x86_64" ]]; then
  cd "$work" && exec "$tools/bin/tlapm" "$@" "${modules[@]}"
fi
exec docker run --rm --user "$(id -u):$(id -g)" -v "$tools:/tlapm:ro" -v "$work:/w" -w /w \
  ubuntu:24.04 /tlapm/bin/tlapm "$@" "${modules[@]}"

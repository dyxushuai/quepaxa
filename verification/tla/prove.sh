#!/usr/bin/env bash
# Checks the TLAPS proof (QuePaxaSafety.tla) with tlapm 1.6.
# Usage: TLAPM=/path/to/tlapm/bin/tlapm verification/tla/prove.sh [tlapm args]
# Without TLAPM, runs a tlapm unpacked under target/tlapm in an ubuntu:24.04 container.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
work=$root/target/tlaps
mkdir -p "$work"
cp "$here"/QuePaxa.tla "$here"/QuePaxaProof.tla "$here"/QuePaxaSafety.tla "$here"/QuePaxaInductive.tla "$work/"
if [[ -n ${TLAPM:-} ]]; then
  cd "$work" && exec "$TLAPM" "$@" QuePaxaSafety.tla QuePaxaInductive.tla
fi
tlapm=$root/target/tlapm
[[ -x $tlapm/bin/tlapm ]] || { echo "unpack tlapm-1.6.0-pre-x86_64-linux-gnu.tar.gz into $tlapm"; exit 2; }
exec docker run --rm -v "$tlapm:/tlapm:ro" -v "$work:/w" -w /w ubuntu:24.04 \
  /tlapm/bin/tlapm "$@" QuePaxaSafety.tla QuePaxaInductive.tla

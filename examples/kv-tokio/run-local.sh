#!/usr/bin/env bash
# Starts a 3-replica kv-tokio cluster on loopback, runs kv-bench against it, stops it.
# Usage: run-local.sh [rate] [secs] [netem-delay-ms]
# With a netem delay (needs `sudo tc`), loopback traffic gets that one-way delay.
set -euo pipefail
rate=${1:-20000}; secs=${2:-10}; delay=${3:-0}
root=$(cd "$(dirname "$0")/../.." && pwd)
cargo build --release -q --manifest-path "$root/Cargo.toml" -p kv-tokio
bin="$root/target/release"
# Data must live on the disk under test: /tmp is often tmpfs, where fsync costs nothing.
data_root=${KV_DATA_ROOT:-$root/target/kv-data}
mkdir -p "$data_root"
data=$(mktemp -d -p "$data_root")
fstype=$(df -T "$data" | awk 'NR==2 {print $2}')
if [ "$fstype" = tmpfs ]; then echo "warning: $data is on tmpfs; fsync is free there" >&2; fi
members="0=127.0.0.1:7100,1=127.0.0.1:7101,2=127.0.0.1:7102"
pids=()
owns_netem=0
cleanup() {
  kill "${pids[@]}" 2>/dev/null || true
  # Remove only the qdisc this run installed.
  if [ "$owns_netem" = 1 ]; then sudo tc qdisc del dev lo root 2>/dev/null || true; fi
  rm -rf "$data"
}
trap cleanup EXIT
if [ "$delay" != 0 ]; then
  sudo tc qdisc add dev lo root netem delay "${delay}ms"
  owns_netem=1
fi
for i in 0 1 2; do
  "$bin/kv-tokio" --id $i --members "$members" --client 127.0.0.1:720$i --data "$data/n$i" 2>"$data/n$i.log" &
  pids+=($!)
done
sleep 1
"$bin/kv-bench" --targets 127.0.0.1:7200,127.0.0.1:7201,127.0.0.1:7202 --rate "$rate" --secs "$secs" --conns 24

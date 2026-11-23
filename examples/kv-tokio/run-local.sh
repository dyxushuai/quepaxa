#!/usr/bin/env bash
# Starts a kv-tokio cluster (3 replicas by default) on loopback, runs kv-bench against it,
# stops it.
# Usage: run-local.sh [rate] [secs] [netem-delay-ms]
# With a netem delay (needs `sudo tc`), loopback traffic gets that one-way delay.
#
# Environment (all optional):
#   KV_SIZE=BYTES        command size (kv-bench --size; default 17)
#   KV_GETS=FRACTION     share of GETs (default 0.5)
#   KV_EGRESS=RATE       caps each replica's egress at a tc rate such as 1gbit. Needs root
#                        through `sudo` (CAP_NET_ADMIN for tc and nft) and a user systemd
#                        session: each replica runs in its own cgroup scope, nft marks the
#                        packets its sockets send, and an HTB class per mark on `lo` limits
#                        them. Other loopback traffic passes unlimited. Without root, run it
#                        in a container or VM where you have it.
#   KV_NODES=N           replicas (default 3, at most 99); replica 0 leads first
#   KV_PORT=BASE         peer ports BASE..BASE+N-1, client ports BASE+100.. (default 7100)
#   KV_CPUS="A B C"      taskset CPU list per replica, e.g. "0-1 2-3 4-5"
#   KV_FLAGS="..."       extra kv-tokio flags for every replica, e.g. "--max-in-flight-slots 1"
#   KV_CONNS=N           kv-bench connections, spread over the replicas (default 24)
#   KV_BENCH_CPUS=LIST   taskset CPU list for kv-bench, e.g. 8-15
#   KV_BIN=DIR           use kv-tokio and kv-bench from DIR instead of building
#   KV_DATA_ROOT=DIR     where replica data lives (default target/kv-data)
# kv-bench prints each replica's STATS at the end: its EGRESS bytes per peer over the
# replica's slots_decided give egress per slot.
set -euo pipefail
rate=${1:-20000}; secs=${2:-10}; delay=${3:-0}
size=${KV_SIZE:-17}; gets=${KV_GETS:-0.5}; egress=${KV_EGRESS:-}; port=${KV_PORT:-7100}
nodes=${KV_NODES:-3}; conns=${KV_CONNS:-24}
read -r -a cpus <<<"${KV_CPUS:-}"
read -r -a flags <<<"${KV_FLAGS:-}"
if [ "$nodes" -lt 1 ] || [ "$nodes" -gt 99 ]; then echo "KV_NODES: 1 to 99, not $nodes" >&2; exit 2; fi
ids=$(seq 0 $((nodes - 1)))
root=$(cd "$(dirname "$0")/../.." && pwd)
if [ -n "${KV_BIN:-}" ]; then
  bin=$KV_BIN
else
  cargo build --release -q --manifest-path "$root/Cargo.toml" -p kv-tokio
  bin="$root/target/release"
fi
# Data must live on the disk under test: /tmp is often tmpfs, where fsync costs nothing.
data_root=${KV_DATA_ROOT:-$root/target/kv-data}
mkdir -p "$data_root"
data=$(mktemp -d -p "$data_root")
fstype=$(df -T "$data" | awk 'NR==2 {print $2}')
if [ "$fstype" = tmpfs ]; then echo "warning: $data is on tmpfs; fsync is free there" >&2; fi
members=$(for i in $ids; do echo "$i=127.0.0.1:$((port + i))"; done | paste -sd,)
targets=$(for i in $ids; do echo "127.0.0.1:$((port + 100 + i))"; done | paste -sd,)
pids=()
owns_qdisc=0
nft_table=kv_local_$$
owns_nft=0
cleanup() {
  kill "${pids[@]}" 2>/dev/null || true
  # Replicas drain their queues before exiting; the next run must not find their ports bound.
  for _ in $(seq 100); do kill -0 "${pids[@]}" 2>/dev/null || break; sleep 0.1; done
  kill -9 "${pids[@]}" 2>/dev/null || true
  wait 2>/dev/null
  # Remove only what this run installed.
  if [ "$owns_qdisc" = 1 ]; then sudo tc qdisc del dev lo root 2>/dev/null || true; fi
  if [ "$owns_nft" = 1 ]; then sudo nft delete table inet "$nft_table" 2>/dev/null || true; fi
  rm -rf "$data"
}
trap cleanup EXIT
fail() {
  echo "FAILED: $*" >&2
  for i in $ids; do echo "--- replica $i log (tail)" >&2; tail -5 "$data/n$i.log" >&2; done
  exit 1
}
ports=()
for i in $ids; do ports+=($((port + i)) $((port + 100 + i))); done
bound() { for p in "${ports[@]}"; do ss -Hltn "sport = :$p" | grep -q . && return 0; done; return 1; }
for _ in $(seq 300); do bound || break; sleep 0.1; done
if bound; then echo "FAILED: ports ${ports[*]} still bound after 30 s:" >&2; ss -Hltnp >&2; exit 1; fi
# Whether the replica serving clients on port $1 answers STATS within a second.
answers() {
  timeout 1 bash -c "exec 3<>/dev/tcp/127.0.0.1/$1 && echo STATS >&3 && head -1 <&3" 2>/dev/null |
    grep -q '^STATS'
}
for i in $ids; do
  launch=()
  if [ -n "$egress" ]; then launch+=(systemd-run --user --scope -q --unit "kv-local-$$-$i"); fi
  if [ -n "${cpus[$i]:-}" ]; then launch+=(taskset -c "${cpus[$i]}"); fi
  # Both exec the command, so the pid is the replica's.
  "${launch[@]}" "$bin/kv-tokio" --id "$i" --members "$members" --client "127.0.0.1:$((port + 100 + i))" \
    --data "$data/n$i" "${flags[@]}" 2>"$data/n$i.log" &
  pids+=($!)
done
if [ -n "$egress" ] || [ "$delay" != 0 ]; then
  sudo tc qdisc add dev lo root handle 1: htb default 99
  owns_qdisc=1
  sudo tc class add dev lo parent 1: classid 1:99 htb rate 100gbit quantum 200000
  classes=(99)
fi
if [ -n "$egress" ]; then
  sleep 0.5 # the scopes exist once their commands run
  sudo nft add table inet "$nft_table"
  owns_nft=1
  sudo nft add chain inet "$nft_table" out '{ type filter hook output priority 0; }'
  for i in $ids; do
    cg=$(cut -d: -f3 "/proc/${pids[$i]}/cgroup"); cg=${cg#/}
    sudo nft add rule inet "$nft_table" out socket cgroupv2 level "$(awk -F/ '{print NF}' <<<"$cg")" \
      "\"$cg\"" meta mark set $((0x11 + i))
    sudo tc class add dev lo parent 1: classid 1:$((11 + i)) htb rate "$egress" ceil "$egress" quantum 200000
    sudo tc filter add dev lo parent 1: protocol all handle $((0x11 + i)) fw flowid 1:$((11 + i))
    classes+=($((11 + i)))
  done
fi
if [ "$delay" != 0 ]; then
  for c in "${classes[@]}"; do sudo tc qdisc add dev lo parent "1:$c" netem delay "${delay}ms"; done
fi
for i in $ids; do
  for _ in $(seq 150); do
    answers $((port + 100 + i)) && break
    kill -0 "${pids[$i]}" 2>/dev/null || break
    sleep 0.2
  done
  answers $((port + 100 + i)) || fail "replica $i does not answer STATS"
done
bench=()
if [ -n "${KV_BENCH_CPUS:-}" ]; then bench+=(taskset -c "$KV_BENCH_CPUS"); fi
"${bench[@]}" "$bin/kv-bench" --targets "$targets" --rate "$rate" --secs "$secs" --conns "$conns" --gets "$gets" \
  --size "$size"
for i in $ids; do kill -0 "${pids[$i]}" 2>/dev/null || fail "replica $i died during the run"; done

#!/usr/bin/env bash
# The paper's evaluation (QuePaxa, SOSP'23, §7) on this host: Fig. 6 (throughput against
# median and p99 latency, LAN and emulated WAN), Fig. 7 (LAN saturation throughput with 3 to
# 13 replicas) and Fig. 10 (convergence to the best leader among heterogeneous machines).
# It drives the existing harness: run-local.sh for the LAN, bench/etcd/wan.sh for the WAN and
# Fig. 10. See docs/benchmarks.md, "The paper's evaluation". Each figure holds the shared
# bench lock from its build to its last run, so no other locked job runs in between.
#
# Usage: paper-eval.sh [--smoke] <fig6-lan|fig7|fig6-wan|fig10>...
# Environment: OUT (raw results, default ~/.local/share/quepaxa-eval/paper-eval), LOCK (the
#   bench lock, default the main checkout's target/ops/bench.lock; - when the caller holds
#   it), REPEATS (default 3); LAN layout: LEADER_CPUS (0-1), FOLLOWER_CPUS (2-11, shared by
#   every follower), BENCH_CPUS (12-15), PORT (21100); LAN_RATES (Fig. 6 LAN offered rates),
#   LAN_SECS (60), RAMP_SECS (20, per Fig. 7 step), SIZES (Fig. 7 replica counts,
#   "3 5 7 9 11 13"); WAN_RATES (Fig. 6 WAN offered rates); LADDER_RATE (40000), LADDER_SECS
#   (240), LADDER_REPLICA_CPUS ("0 1-2 3-4 5-7 8-11") and LADDER_CPUS ("1 1.5 2 3 4": the
#   quota on each cpuset; the second member's makes it 1.5 CPUs), LADDER_GEN_CPUS (12-15);
#   SNAPSHOT_EVERY (kv-tokio --snapshot-every for every figure, default 10^12: never, as the
#   paper's prototype does no log compaction, §7.1 footnote 8; 10000 is kv-tokio's default).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
out=${OUT:-$HOME/.local/share/quepaxa-eval/paper-eval}
# The main checkout's lock, also from a worktree: every checkout's runs share one host.
main=$(dirname "$(git -C "$root" rev-parse --path-format=absolute --git-common-dir)")
lock=${LOCK:-$main/target/ops/bench.lock}
repeats=${REPEATS:-3}
leader_cpus=${LEADER_CPUS:-0-1} follower_cpus=${FOLLOWER_CPUS:-2-11} bench_cpus=${BENCH_CPUS:-12-15}
# Below the ephemeral range (32768-60999 here), where outgoing connections can take a port.
port=${PORT:-21100}
lan_rates=${LAN_RATES:-50000 100000 200000 300000 400000 500000 600000}
lan_secs=${LAN_SECS:-60} ramp_secs=${RAMP_SECS:-20}
sizes=${SIZES:-3 5 7 9 11 13}
wan_rates=${WAN_RATES:-1000 5000 10000 20000 30000 40000 60000 100000}
ladder_rate=${LADDER_RATE:-40000} ladder_secs=${LADDER_SECS:-240}
# Fig. 10's machines: one cpuset per replica (us-east, us-west, eu-west, ap-ne, sa-east), the
# smallest leading first.
ladder_replica_cpus=${LADDER_REPLICA_CPUS:-0 1-2 3-4 5-7 8-11} ladder_cpus=${LADDER_CPUS:-1 1.5 2 3 4}
ladder_gen_cpus=${LADDER_GEN_CPUS:-12-15}
snapshots="--snapshot-every ${SNAPSHOT_EVERY:-1000000000000}"
# Fig. 7's bound: the paper's saturation point in Fig. 6a.
median_bound_us=5800

smoke=0
[ "${1:-}" = --smoke ] && { smoke=1; shift; }
if [ $smoke = 1 ]; then
  repeats=1 lan_rates="100000 400000" lan_secs=5 ramp_secs=5 sizes="3 13" wan_rates="1000 50000"
  ladder_secs=40
fi
[ $# -gt 0 ] || { sed -n '9,18p' "$0" >&2; exit 2; }
mkdir -p "$out"
[ "$lock" = - ] || mkdir -p "$(dirname "$lock")"
stamp=$(date +%Y%m%d-%H%M%S)

# The host, for env.txt: CPUs, memory, the data filesystem and the commit.
describe() {
  echo "date: $(date -u +%FT%TZ)"
  echo "commit: $(git -C "$root" rev-parse HEAD)$(git -C "$root" diff --quiet HEAD || echo ' (dirty)')"
  echo "kernel: $(uname -sr); nproc: $(nproc); load: $(cut -d' ' -f1-3 /proc/loadavg)"
  echo "memory: $(free -g | awk '/^Mem:/ { print $2 " GiB" }')"
  lscpu | sed -n 's/^\(Model name\|CPU(s)\|Thread(s) per core\|Core(s) per socket\|CPU max MHz\):\s*/\1: /p'
}

# Builds kv-tokio and kv-bench once per run of this script.
bin=""
build() {
  [ -n "$bin" ] && return
  bin=$out/bin-$stamp
  mkdir -p "$bin"
  cargo build --release -q --manifest-path "$root/Cargo.toml" -p kv-tokio --bins
  cp "$root/target/release/kv-tokio" "$root/target/release/kv-bench" "$bin/"
}

# lan <dir> <name> <nodes> <rate> <secs> [kv-tokio flags]: one run-local.sh run on tmpfs, the
# leader (replica 0) on its own CPUs, the followers sharing theirs; output in <dir>/<name>.txt.
lan() {
  local dir=$1 name=$2 nodes=$3 rate=$4 secs=$5 flags=${6:-} cpus="$leader_cpus"
  for _ in $(seq 2 "$nodes"); do cpus+=" $follower_cpus"; done
  KV_BIN=$bin KV_NODES=$nodes KV_CPUS=$cpus KV_BENCH_CPUS=$bench_cpus KV_PORT=$port KV_GETS=0 \
    KV_DATA_ROOT=/dev/shm/qpx-paper-eval KV_FLAGS="$snapshots $flags" \
    "$root/examples/kv-tokio/run-local.sh" "$rate" "$secs" > "$dir/$name.txt" 2>&1
}

# field <file> <key>: a value of kv-bench's summary line, e.g. p50 in us.
field() { awk -v k="$2" '/^offered=/ { for (i = 1; i <= NF; i++) if (index($i, k "=") == 1) { v = substr($i, length(k) + 2); sub(/(us|\/s)$/, "", v); print v } }' "$1"; }

# Whether a run sustained its offered rate under the paper's median bound: every request
# answered, at least 97% of the offered rate, and p50 at most 5.8 ms.
sustained() {
  local f=$1 offered=$2
  awk -v t="$(field "$f" throughput)" -v o="$offered" -v e="$(field "$f" errors)" -v u="$(field "$f" unanswered)" \
    -v m="$(field "$f" missed)" -v p="$(field "$f" p50)" -v b="$median_bound_us" \
    'BEGIN { exit !(t != "" && t >= 0.97 * o && e == 0 && u == 0 && m == 0 && p <= b) }'
}

fig6_lan() {
  local dir=$out/fig6-lan-$stamp run rate
  mkdir -p "$dir"; describe > "$dir/env.txt"
  echo "layout: 3 replicas, leader $leader_cpus, followers $follower_cpus, kv-bench $bench_cpus; tmpfs; all PUTs; ${lan_secs}s per point; $snapshots" >> "$dir/env.txt"
  for run in $(seq 1 "$repeats"); do
    for rate in $lan_rates; do
      echo "fig6-lan run $run: $rate/s" >&2
      lan "$dir" "n3-$rate-$run" 3 "$rate" "$lan_secs"
    done
  done
  echo "$dir"
}

# ramp <dir> <nodes> <run> [flags] [tag]: offered rate up by 100k/s from 100k/s until a step
# is not sustained, then the midpoint between the last sustained step and it, once. Appends
# "<tag> <run> <highest sustained rate>" to capacity.txt.
ramp() {
  local dir=$1 nodes=$2 run=$3 flags=${4:-} tag=${5:-n$2} rate=100000 best=0 fail=0 mid
  while [ "$rate" -le 1200000 ]; do
    lan "$dir" "$tag-$rate-$run" "$nodes" "$rate" "$ramp_secs" "$flags"
    if sustained "$dir/$tag-$rate-$run.txt" "$rate"; then best=$rate; rate=$((rate + 100000)); else fail=$rate; break; fi
  done
  if [ "$fail" -gt 0 ] && [ "$best" -gt 0 ]; then
    mid=$(((best + fail) / 2))
    lan "$dir" "$tag-$mid-$run" "$nodes" "$mid" "$ramp_secs" "$flags"
    sustained "$dir/$tag-$mid-$run.txt" "$mid" && best=$mid
  fi
  echo "$tag $run $best" >> "$dir/capacity.txt"
}

fig7() {
  local dir=$out/fig7-$stamp run n
  mkdir -p "$dir"; describe > "$dir/env.txt"
  echo "layout: leader $leader_cpus, followers share $follower_cpus, kv-bench $bench_cpus; tmpfs; all PUTs; ${ramp_secs}s per step; bound p50 <= ${median_bound_us}us; $snapshots" >> "$dir/env.txt"
  for run in $(seq 1 "$repeats"); do
    for n in $sizes; do
      echo "fig7 run $run: $n replicas" >&2
      ramp "$dir" "$n" "$run"
    done
    # The paper used no pipelining on the LAN: the same ramp with one slot in flight.
    echo "fig7 run $run: 3 replicas, one slot in flight" >&2
    ramp "$dir" 3 "$run" "--max-in-flight-slots 1" "n3-pipe1"
  done
  echo "$dir"
}

fig6_wan() {
  local dir=$out/fig6-wan-$stamp smokeflag=()
  [ $smoke = 1 ] && smokeflag=(--smoke)
  mkdir -p "$dir"
  SYSTEMS=quepaxa DATA_TMPFS=1 REPEATS=$repeats LOAD_RATES=$wan_rates RESULTS_DIR=$dir \
    QUEPAXA_FLAGS="--max-in-flight-slots 10 $snapshots" \
    "$here/etcd/wan.sh" "${smokeflag[@]}" load > "$dir/wan.log" 2>&1
  describe > "$dir/host.txt"
  echo "$dir"
}

fig10() {
  local base=$out/fig10-$stamp policy dir smokeflag=()
  [ $smoke = 1 ] && smokeflag=(--smoke)
  for policy in on off; do
    dir=$base/placement-$policy
    mkdir -p "$dir"
    SYSTEMS=quepaxa DATA_TMPFS=1 REPEATS=$repeats RATE=$ladder_rate RESULTS_DIR=$dir SAMPLE_S=1 \
      REPLICA_CPUS=$ladder_replica_cpus LADDER_CPUS=$ladder_cpus GEN_CPUS=$ladder_gen_cpus \
      POLICY="--placement $policy --placement-periods 10000,10000" POLICY_SECS=$ladder_secs \
      SETTLED_FROM=$((ladder_secs * 2 / 3)) QUEPAXA_FLAGS=$snapshots \
      "$here/etcd/wan.sh" "${smokeflag[@]}" ladder > "$dir/wan.log" 2>&1
  done
  describe > "$base/host.txt"
  echo "$base"
}

for a in "$@"; do
  case $a in fig6-lan | fig7 | fig6-wan | fig10) ;; *) echo "unknown argument $a" >&2; exit 2 ;; esac
done
for a in "$@"; do
  if [ "$lock" != - ]; then
    echo "$a: waiting for $lock" >&2
    exec 9> "$lock"
    flock 9
  fi
  case $a in
    fig6-lan) build; dir=$(fig6_lan | tail -1) ;;
    fig7) build; dir=$(fig7 | tail -1) ;;
    fig6-wan) dir=$(fig6_wan | tail -1) ;;
    fig10) dir=$(fig10 | tail -1) ;;
  esac
  if [ "$lock" != - ]; then flock -u 9; exec 9>&-; fi
  "$here/paper-summarize.sh" "$dir" > "$dir/summary.md"
  echo "results: $dir"
  cat "$dir/summary.md"
done

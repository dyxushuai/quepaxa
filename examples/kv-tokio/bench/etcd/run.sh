#!/usr/bin/env bash
# Compares kv-tokio (QuePaxa), openraft-kv (Openraft), and etcd in containers on this host,
# all driven through etcd's v3 KV API. See README.md.
#
# Usage: run.sh [--smoke] [scenario...]
#   scenarios: steady mixed load crash partition slow-disk stuck-disk far-leader
#     (default: steady mixed crash partition slow-disk stuck-disk far-leader)
#   --smoke    short runs, to check the harness works
# Environment: NODES (cluster size, 3 or 5, default 3), SYSTEMS (default "quepaxa etcd
#   etcd-tuned"; also supports "quepaxa-auto", "openraft", "openraft-tuned"), REPEATS
#   (default 1), INTERLEAVE (0 or 1, default 0), RATE (fault-scenario load, default 5000/s),
#   SIZE (kv-bench payload size in bytes, default 17), CONNS (gRPC connections, default 16),
#   LOAD_RATES (open-loop PUT rates for load scenario, default "5000 15000 30000 50000
#   100000"), LOAD_SECS (default 20), SNAPSHOT_EVERY (if set, passed to kv-tokio and
#   openraft-kv), QUEPAXA_FLAGS, OPENRAFT_FLAGS, DATA_TMPFS (0 or 1, default 0), NODE_CPUS
#   ("0-2 3-5 6-8"; with 5 nodes "0-1 2-3 4-5 6-7 8-9"), GEN_CPUS (9-12; with 5 nodes
#   10-13), PROBE_CPUS (default GEN_CPUS), SLOW_IOPS (default 50), FAR_MS (default 50),
#   MIXED_RATE (default 40000/s), RESULTS_DIR (default target/bench-etcd/<time>/).
# Needs Docker (with cgroup v2 for slow-disk) and jq.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)

smoke=0
scenarios=()
for a in "$@"; do
  case $a in
    --smoke) smoke=1 ;;
    steady | mixed | load | crash | partition | slow-disk | stuck-disk | far-leader) scenarios+=("$a") ;;
    *) echo "unknown argument $a" >&2; exit 2 ;;
  esac
done
[ ${#scenarios[@]} -gt 0 ] || scenarios=(steady mixed crash partition slow-disk stuck-disk far-leader)

if [ $smoke = 1 ]; then
  secs=8 fault=3 heal=6 total=2000 steady_secs=2 repeats=1 load_secs=${LOAD_SECS:-6}
else
  secs=40 fault=10 heal=25 total=100000 steady_secs=20 repeats=${REPEATS:-1} load_secs=${LOAD_SECS:-20}
fi
rate=${RATE:-5000}
n=${NODES:-3}
case $n in
  3) default_cpus="0-2 3-5 6-8" default_gen=9-12 ;;
  5) default_cpus="0-1 2-3 4-5 6-7 8-9" default_gen=10-13 ;;
  *) echo "NODES must be 3 or 5" >&2; exit 2 ;;
esac
read -r -a node_cpus <<< "${NODE_CPUS:-$default_cpus}"
gen_cpus=${GEN_CPUS:-$default_gen}
probe_cpus=${PROBE_CPUS:-$gen_cpus}
ids=$(seq 0 $((n - 1)))
slow_iops=${SLOW_IOPS:-50}
far_ms=${FAR_MS:-50}
mixed_rate=${MIXED_RATE:-40000}
size=${SIZE:-17}
conns=${CONNS:-16}
interleave=${INTERLEAVE:-0}
data_tmpfs=${DATA_TMPFS:-0}
snapshot_every=${SNAPSHOT_EVERY:-}
read -r -a load_rates <<< "${LOAD_RATES:-5000 15000 30000 50000 100000}"
read -r -a quepaxa_flags <<< "${QUEPAXA_FLAGS:-}"
read -r -a openraft_flags <<< "${OPENRAFT_FLAGS:-}"
read -r -a systems <<< "${SYSTEMS:-quepaxa etcd etcd-tuned}"
openraft_hb=${OPENRAFT_HEARTBEAT_MS:-50}
openraft_emin=${OPENRAFT_ELECTION_MIN_MS:-150}
openraft_emax=${OPENRAFT_ELECTION_MAX_MS:-300}
openraft_tuned_hb=${OPENRAFT_TUNED_HEARTBEAT_MS:-20}
openraft_tuned_emin=${OPENRAFT_TUNED_ELECTION_MIN_MS:-60}
openraft_tuned_emax=${OPENRAFT_TUNED_ELECTION_MAX_MS:-100}

net=qpx-bench
prefix=172.31.250
results=${RESULTS_DIR:-$root/target/bench-etcd/$(date +%Y%m%d-%H%M%S)}
# On the disk under test, never tmpfs unless DATA_TMPFS=1: fsync must cost what it costs.
data=$root/target/bench-etcd/data
mkdir -p "$results" "$data"
if [ "$data_tmpfs" != 1 ] && [ "$(df -T "$data" | awk 'NR==2 {print $2}')" = tmpfs ]; then
  echo "warning: $data is on tmpfs; fsync is free there" >&2
fi
# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$here/lib.sh"

nodes=()
cleanup() {
  docker rm -f "${nodes[@]}" qpx-gen qpx-disk >/dev/null 2>&1 || true
  wipe_data 2>/dev/null || true
}
trap 'cleanup; docker network rm "$net" >/dev/null 2>&1 || true' EXIT

need_openraft=0
for s in "${systems[@]}"; do [[ $s == openraft* ]] && need_openraft=1; done
build_images
[ "$need_openraft" = 1 ] && build_openraft_image
docker network inspect "$net" >/dev/null 2>&1 || docker network create --subnet "$prefix.0/24" "$net" >/dev/null

{
  describe_host
  echo "systems: ${systems[*]}"
  echo "nodes: $n, rate: $rate/s, size: ${size}B, conns: $conns, run ${secs}s, fault at ${fault}s, heal at ${heal}s, repeats $repeats, interleave=$interleave, smoke=$smoke"
  echo "load rates: ${load_rates[*]} for ${load_secs}s each; snapshot_every: ${snapshot_every:-default}"
  echo "quepaxa flags: ${quepaxa_flags[*]:-none}; openraft flags: ${openraft_flags[*]:-none}"
  echo "cpus: nodes ${node_cpus[*]}, generator $gen_cpus, disk probe $probe_cpus"
  if [ "$data_tmpfs" = 1 ]; then echo "data: tmpfs"; else echo "data: host disk"; fi
  echo "timers: etcd 100/1000ms; etcd-tuned 10/100ms; openraft ${openraft_hb}/${openraft_emin}-${openraft_emax}ms; openraft-tuned ${openraft_tuned_hb}/${openraft_tuned_emin}-${openraft_tuned_emax}ms"
} > "$results/env.txt"
cat "$results/env.txt"

data_mount() {
  if [ "$data_tmpfs" = 1 ]; then echo "--tmpfs /data"; else echo "-v $data/n$1:/data"; fi
}

# The members' addresses on `port`: `<sep><id>=<ip>:<port>` joined by commas.
member_list() {
  local i out=
  for i in $ids; do out+="${out:+,}$1$i=${2:-}$prefix.1$i:$3"; done
  echo "$out"
}

endpoint_list() {
  local i out=
  for i in $ids; do out+="${out:+,}$prefix.1$i:2379"; done
  echo "$out"
}

# start_quepaxa [extra_flags...]: `n` kv-tokio replicas; replica 0 leads (smallest id).
start_quepaxa() {
  local members i mount extra=("$@") snap=()
  [ -n "$snapshot_every" ] && snap=(--snapshot-every "$snapshot_every")
  members=$(member_list "" "" 7000)
  nodes=()
  for i in $ids; do nodes+=("qpx-n$i"); done
  for i in $ids; do
    read -r -a mount <<< "$(data_mount "$i")"
    docker run -d --name "qpx-n$i" --network "$net" --ip "$prefix.1$i" --cpuset-cpus "${node_cpus[$i]}" \
      "${mount[@]}" "$image" \
      kv-tokio --id "$i" --members "$members" --client 0.0.0.0:7100 --etcd 0.0.0.0:2379 --data /data \
      "${snap[@]}" "${extra[@]}" "${quepaxa_flags[@]}" >/dev/null
  done
  leader=qpx-n0
  followers=()
  for i in $ids; do [ "$i" = 0 ] || followers+=("$prefix.1$i"); done
  endpoints=$(endpoint_list)
}

# start_etcd <heartbeat ms> <election ms>; the leader is whoever etcd elects.
start_etcd() {
  local cluster i mount
  cluster=$(member_list n http:// 2380)
  nodes=()
  for i in $ids; do nodes+=("qpx-n$i"); done
  for i in $ids; do
    read -r -a mount <<< "$(data_mount "$i")"
    docker run -d --name "qpx-n$i" --network "$net" --ip "$prefix.1$i" --cpuset-cpus "${node_cpus[$i]}" \
      "${mount[@]}" "$etcd_image" \
      etcd --name "n$i" --data-dir /data \
      --listen-peer-urls http://0.0.0.0:2380 --initial-advertise-peer-urls "http://$prefix.1$i:2380" \
      --listen-client-urls http://0.0.0.0:2379 --advertise-client-urls "http://$prefix.1$i:2379" \
      --initial-cluster "$cluster" --initial-cluster-state new \
      --heartbeat-interval "$1" --election-timeout "$2" >/dev/null
  done
  endpoints=$(endpoint_list)
}

# start_openraft <heartbeat ms> <election min ms> <election max ms>
start_openraft() {
  local members i mount snap=()
  [ -n "$snapshot_every" ] && snap=(--snapshot-every "$snapshot_every")
  members=$(member_list "" "" 7000)
  nodes=()
  for i in $ids; do nodes+=("qpx-n$i"); done
  for i in $ids; do
    read -r -a mount <<< "$(data_mount "$i")"
    docker run -d --name "qpx-n$i" --network "$net" --ip "$prefix.1$i" --cpuset-cpus "${node_cpus[$i]}" \
      "${mount[@]}" "$openraft_image" \
      openraft-kv --id "$i" --members "$members" --peer 0.0.0.0:7000 --etcd 0.0.0.0:2379 --data /data \
      --heartbeat-ms "$1" --election-min-ms "$2" --election-max-ms "$3" \
      "${snap[@]}" "${openraft_flags[@]}" >/dev/null
  done
  endpoints=$(endpoint_list)
}

openraft_leader() {
  local i lid
  for i in $ids; do
    lid=$(docker exec "qpx-n$i" openraft-kv ctl --endpoint "$prefix.1$i:7000" leader 2>/dev/null || true)
    [[ $lid =~ ^[0-9]+$ ]] && { echo "$lid"; return; }
  done
}

# Waits until the cluster takes writes, then identifies the leader if needed.
ready() {
  local system=$1 deadline=$((SECONDS + 90))
  if [[ $system == openraft* ]]; then
    until docker exec qpx-n0 openraft-kv ctl --endpoint "$prefix.10:7000" init >/dev/null 2>&1; do
      [ $SECONDS -lt $deadline ] || { echo "openraft init failed" >&2; exit 1; }
      sleep 1
    done
  fi
  until docker run --rm --network "$net" "$image" benchmark --endpoints="$endpoints" \
    put --total 10 >/dev/null 2>&1; do
    [ $SECONDS -lt $deadline ] || { echo "cluster did not come up" >&2; exit 1; }
    sleep 1
  done
  if [[ $system == openraft* ]]; then
    local lid i
    lid=$(openraft_leader)
    [ -n "$lid" ] || { echo "openraft leader not found" >&2; exit 1; }
    leader="qpx-n$lid"
    followers=()
    for i in $ids; do [ "$i" = "$lid" ] || followers+=("$prefix.1$i"); done
  elif [[ $system == etcd* ]]; then
    local status leader_ip i
    status=$(docker exec qpx-n0 etcdctl --endpoints="$endpoints" endpoint status -w json)
    leader_ip=$(jq -r '.[] | select(.Status.header.member_id == .Status.leader) | .Endpoint' <<< "$status" |
      sed 's|http://||; s|:2379||')
    leader="qpx-n${leader_ip: -1}"
    followers=()
    for i in $ids; do [ "$prefix.1$i" = "$leader_ip" ] || followers+=("$prefix.1$i"); done
  fi
  echo "  leader: $leader, followers: ${followers[*]}"
}

stop() {
  docker rm -f "${nodes[@]}" >/dev/null 2>&1 || true
  wipe_data
}

inject() {
  case $1 in
    crash) docker kill -s KILL "$leader" >/dev/null ;;
    partition)
      sidecar "$leader" "for f in ${followers[*]}; do iptables -A INPUT -s \$f -j DROP; iptables -A OUTPUT -d \$f -j DROP; done" ;;
    slow-disk | stuck-disk)
      local cg iops=$slow_iops
      [ "$1" = stuck-disk ] && iops=1
      cg=$(cgroup_of "$leader")
      sidecar "$leader" "echo '$(findmnt -no MAJ:MIN -T "$data") wiops=$iops' > $cg/io.max" ;;
    far-leader) sidecar "$leader" "tc qdisc add dev eth0 root netem delay ${far_ms}ms" ;;
  esac
}

heal() {
  case $1 in
    crash) ;;
    partition) sidecar "$leader" "iptables -F" ;;
    slow-disk | stuck-disk)
      local cg
      cg=$(cgroup_of "$leader")
      sidecar "$leader" "echo '$(findmnt -no MAJ:MIN -T "$data") wiops=max' > $cg/io.max" ;;
    far-leader) sidecar "$leader" "tc qdisc del dev eth0 root" ;;
  esac
}

# The fault scenarios: open-loop load on the followers (on every member when the leader's
# disk is stuck: its own clients are what that costs), fault, heal, recovery.
faulted() {
  local name=$1 scenario=$2 run=$3 targets prober=""
  local out="$results/$name"
  targets=$(printf '%s:2379,' "${followers[@]}" | sed 's/,$//')
  [ "$scenario" = stuck-disk ] && targets=$endpoints
  local t0=$(($(date +%s%3N) + 5000))
  if [ "$data_tmpfs" != 1 ]; then
    probe_disk "$t0" "$secs" "$name-disk.txt" "$probe_cpus" &
    prober=$!
  fi
  docker run --rm --name qpx-gen --network "$net" --cpuset-cpus "$gen_cpus" -v "$results:/out" "$image" \
    kv-bench --protocol etcd --targets "$targets" --rate "$rate" --size "$size" \
    --gets 0 --secs "$secs" --conns "$conns" --start-at "$t0" --fault-at "$fault" --timeline 250 \
    --seed $((run * 17 + 1)) --samples "/out/$name.csv" > "$out.txt" &
  local gen=$!
  sleep_until $((t0 + fault * 1000))
  inject "$scenario"
  sleep_until $((t0 + heal * 1000))
  heal "$scenario"
  wait $gen
  [ -z "$prober" ] || wait "$prober"
  head -2 "$out.txt" | sed 's/^/    /'
}

# Open-loop 100% PUTs across all members at one offered rate (latency-vs-throughput curve).
run_load() {
  local name=$1 offered=$2 run=$3 prober=""
  local out="$results/$name"
  local t0=$(($(date +%s%3N) + 5000))
  if [ "$data_tmpfs" != 1 ]; then
    probe_disk "$t0" "$load_secs" "$name-disk.txt" "$probe_cpus" &
    prober=$!
  fi
  docker run --rm --name qpx-gen --network "$net" --cpuset-cpus "$gen_cpus" -v "$results:/out" "$image" \
    kv-bench --protocol etcd --targets "$endpoints" --rate "$offered" --size "$size" \
    --gets 0 --secs "$load_secs" --conns "$conns" --start-at "$t0" \
    --seed $((run * 17 + 1)) --samples "/out/$name.csv" > "$out.txt"
  [ -z "$prober" ] || wait "$prober"
  head -1 "$out.txt" | sed 's/^/    /'
}

# A read-heavy open-loop load (90% linearizable ranges, 10% puts) on all members.
mixed() {
  local name=$1 run=$2 prober=""
  local t0=$(($(date +%s%3N) + 5000))
  if [ "$data_tmpfs" != 1 ]; then
    probe_disk "$t0" "$secs" "$name-disk.txt" "$probe_cpus" &
    prober=$!
  fi
  docker run --rm --name qpx-gen --network "$net" --cpuset-cpus "$gen_cpus" "$image" \
    kv-bench --protocol etcd --targets "${endpoints}" --rate "$mixed_rate" --size "$size" --gets 0.9 \
    --secs "$secs" --conns "$conns" --start-at "$t0" --seed $((run * 17 + 1)) > "$results/$name.txt"
  [ -z "$prober" ] || wait "$prober"
  head -1 "$results/$name.txt" | sed 's/^/    /'
}

# Steady state with etcd's benchmark tool, against all members.
steady() {
  local name=$1
  local out="$results/$name.txt" bench=(docker run --rm --network "$net" --cpuset-cpus "$gen_cpus" "$image"
    benchmark --endpoints="$endpoints" --conns "$conns" --clients 256)
  {
    echo "## put, unlimited"; "${bench[@]}" put --total "$total" --key-space-size 100000
    echo "## put, $rate/s"; "${bench[@]}" put --total $((rate * steady_secs)) --rate "$rate" --key-space-size 100000
    # The key the ranges read (etcd's benchmark writes random keys).
    docker run --rm --network "$net" "$etcd_image" etcdctl --endpoints="$endpoints" put foo bar >/dev/null
    echo "## range linearizable"; "${bench[@]}" range foo --total "$total" --consistency l
    echo "## range serializable"; "${bench[@]}" range foo --total "$total" --consistency s
    echo "## txn-put"; "${bench[@]}" txn-put --total "$total"
  } > "$out" 2>&1
  awk 'match($0, /## .*/) {w = substr($0, RSTART + 3)} /Requests\/sec/ {printf "    %s: %.0f/s\n", w, $2}' "$out"
}

skip_case() {
  local system=$1 scenario=$2
  case $system in
    etcd-tuned | openraft-tuned)
      case $scenario in steady | mixed | load) return 0 ;; esac ;;
  esac
  return 1
}

run_trial() {
  local system=$1 scenario=$2 label=$3 run=$4 offered=${5:-}
  local name="$system-$label-$run"
  echo "$system: $label, run $run"
  case $system in
    quepaxa) start_quepaxa ;;
    quepaxa-auto) start_quepaxa --tune-in-flight-slots on --max-in-flight-slots 16 ;;
    etcd) start_etcd 100 1000 ;;
    etcd-tuned) start_etcd 10 100 ;;
    openraft) start_openraft "$openraft_hb" "$openraft_emin" "$openraft_emax" ;;
    openraft-tuned) start_openraft "$openraft_tuned_hb" "$openraft_tuned_emin" "$openraft_tuned_emax" ;;
    *) echo "unknown system $system" >&2; exit 2 ;;
  esac
  ready "$system"
  case $scenario in
    steady) steady "$name" ;;
    mixed) mixed "$name" "$run" ;;
    load) run_load "$name" "$offered" "$run" ;;
    *) faulted "$name" "$scenario" "$run" ;;
  esac
  stop
}

if [ "$interleave" = 1 ]; then
  for scenario in "${scenarios[@]}"; do
    rates=("")
    [ "$scenario" = load ] && rates=("${load_rates[@]}")
    for offered in "${rates[@]}"; do
      label=$scenario
      [ "$scenario" = load ] && label="load-$offered"
      for run in $(seq 1 "$repeats"); do
        for system in "${systems[@]}"; do
          skip_case "$system" "$scenario" && continue
          run_trial "$system" "$scenario" "$label" "$run" "$offered"
        done
      done
    done
  done
else
  for system in "${systems[@]}"; do
    for scenario in "${scenarios[@]}"; do
      skip_case "$system" "$scenario" && continue
      rates=("")
      [ "$scenario" = load ] && rates=("${load_rates[@]}")
      for offered in "${rates[@]}"; do
        label=$scenario
        [ "$scenario" = load ] && label="load-$offered"
        for run in $(seq 1 "$repeats"); do
          run_trial "$system" "$scenario" "$label" "$run" "$offered"
        done
      done
    done
  done
fi
"$here/summarize.sh" "$results" > "$results/summary.md"
echo "results: $results"
cat "$results/summary.md"

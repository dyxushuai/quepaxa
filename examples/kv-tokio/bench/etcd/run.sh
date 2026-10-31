#!/usr/bin/env bash
# Compares a 3-replica kv-tokio cluster (QuePaxa) with a 3-member etcd cluster, both driven
# through etcd's v3 KV API, in containers on this host. See README.md.
#
# Usage: run.sh [--smoke] [scenario...]
#   scenarios: steady mixed crash partition slow-disk far-leader (default: all)
#   --smoke    short runs, to check the harness works
# Environment: RATE (fault-scenario load, default 5000/s), NODE_CPUS ("0-2 3-5 6-8"),
#   GEN_CPUS (9-12), SLOW_IOPS (write IOPS cap on the slow leader, default 50),
#   FAR_MS (one-way delay added to the far leader, default 50), MIXED_RATE (read-heavy load,
#   default 40000/s).
# Needs Docker (with cgroup v2 for slow-disk) and jq. Results go to target/bench-etcd/<time>/.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)

smoke=0
scenarios=()
for a in "$@"; do
  case $a in
    --smoke) smoke=1 ;;
    steady | mixed | crash | partition | slow-disk | far-leader) scenarios+=("$a") ;;
    *) echo "unknown argument $a" >&2; exit 2 ;;
  esac
done
[ ${#scenarios[@]} -gt 0 ] || scenarios=(steady mixed crash partition slow-disk far-leader)

if [ $smoke = 1 ]; then
  secs=8 fault=3 heal=6 total=2000 steady_secs=2
else
  secs=40 fault=10 heal=25 total=100000 steady_secs=20
fi
rate=${RATE:-5000}
read -r -a node_cpus <<< "${NODE_CPUS:-0-2 3-5 6-8}"
gen_cpus=${GEN_CPUS:-9-12}
slow_iops=${SLOW_IOPS:-50}
far_ms=${FAR_MS:-50}
mixed_rate=${MIXED_RATE:-40000}

net=qpx-bench
prefix=172.31.250
results=$root/target/bench-etcd/$(date +%Y%m%d-%H%M%S)
# On the disk under test, never tmpfs: fsync must cost what it costs.
data=$root/target/bench-etcd/data
mkdir -p "$results" "$data"
if [ "$(df -T "$data" | awk 'NR==2 {print $2}')" = tmpfs ]; then
  echo "warning: $data is on tmpfs; fsync is free there" >&2
fi
# shellcheck source=lib.sh
source "$here/lib.sh"

nodes=()
cleanup() {
  docker rm -f "${nodes[@]}" qpx-gen >/dev/null 2>&1 || true
  wipe_data 2>/dev/null || true
}
trap 'cleanup; docker network rm "$net" >/dev/null 2>&1 || true' EXIT

build_images
docker network inspect "$net" >/dev/null 2>&1 || docker network create --subnet "$prefix.0/24" "$net" >/dev/null

{
  describe_host
  echo "rate: $rate/s, run ${secs}s, fault at ${fault}s, heal at ${heal}s, smoke=$smoke"
  echo "cpus: nodes ${node_cpus[*]}, generator $gen_cpus"
} > "$results/env.txt"
cat "$results/env.txt"

# start_quepaxa: three kv-tokio replicas; replica 0 leads (smallest id).
start_quepaxa() {
  local members="0=$prefix.10:7000,1=$prefix.11:7000,2=$prefix.12:7000"
  nodes=(qpx-n0 qpx-n1 qpx-n2)
  for i in 0 1 2; do
    docker run -d --name "qpx-n$i" --network "$net" --ip "$prefix.1$i" --cpuset-cpus "${node_cpus[$i]}" \
      -v "$data/n$i:/data" "$image" \
      kv-tokio --id "$i" --members "$members" --client 0.0.0.0:7100 --etcd 0.0.0.0:2379 --data /data >/dev/null
  done
  leader=qpx-n0
  followers=("$prefix.11" "$prefix.12")
  endpoints="$prefix.10:2379,$prefix.11:2379,$prefix.12:2379"
}

# start_etcd <heartbeat ms> <election ms>; the leader is whoever etcd elects.
start_etcd() {
  local cluster="n0=http://$prefix.10:2380,n1=http://$prefix.11:2380,n2=http://$prefix.12:2380"
  nodes=(qpx-n0 qpx-n1 qpx-n2)
  for i in 0 1 2; do
    docker run -d --name "qpx-n$i" --network "$net" --ip "$prefix.1$i" --cpuset-cpus "${node_cpus[$i]}" \
      -v "$data/n$i:/data" "$etcd_image" \
      etcd --name "n$i" --data-dir /data \
      --listen-peer-urls http://0.0.0.0:2380 --initial-advertise-peer-urls "http://$prefix.1$i:2380" \
      --listen-client-urls http://0.0.0.0:2379 --advertise-client-urls "http://$prefix.1$i:2379" \
      --initial-cluster "$cluster" --initial-cluster-state new \
      --heartbeat-interval "$1" --election-timeout "$2" >/dev/null
  done
  endpoints="$prefix.10:2379,$prefix.11:2379,$prefix.12:2379"
}

# Waits until the cluster takes writes, then finds etcd's leader if needed.
ready() {
  local deadline=$((SECONDS + 60))
  until docker run --rm --network "$net" "$image" benchmark --endpoints="$endpoints" \
    put --total 10 >/dev/null 2>&1; do
    [ $SECONDS -lt $deadline ] || { echo "cluster did not come up" >&2; exit 1; }
    sleep 1
  done
  if [ "$1" != quepaxa ]; then
    local status leader_ip
    status=$(docker exec qpx-n0 etcdctl --endpoints="$endpoints" endpoint status -w json)
    leader_ip=$(jq -r '.[] | select(.Status.header.member_id == .Status.leader) | .Endpoint' <<< "$status" |
      sed 's|http://||; s|:2379||')
    leader="qpx-n${leader_ip: -1}"
    followers=()
    for i in 0 1 2; do [ "$prefix.1$i" = "$leader_ip" ] || followers+=("$prefix.1$i"); done
  fi
  echo "  leader: $leader, followers: ${followers[*]}"
}

stop() {
  docker rm -f "${nodes[@]}" >/dev/null 2>&1 || true
  wipe_data
}

inject() {
  local f1=${followers[0]} f2=${followers[1]}
  case $1 in
    crash) docker kill -s KILL "$leader" >/dev/null ;;
    partition)
      sidecar "$leader" "for f in $f1 $f2; do iptables -A INPUT -s \$f -j DROP; iptables -A OUTPUT -d \$f -j DROP; done" ;;
    slow-disk)
      local cg
      cg=$(cgroup_of "$leader")
      sidecar "$leader" "echo '$(findmnt -no MAJ:MIN -T "$data") wiops=$slow_iops' > $cg/io.max" ;;
    far-leader) sidecar "$leader" "tc qdisc add dev eth0 root netem delay ${far_ms}ms" ;;
  esac
}

heal() {
  case $1 in
    crash) ;;
    partition) sidecar "$leader" "iptables -F" ;;
    slow-disk)
      local cg
      cg=$(cgroup_of "$leader")
      sidecar "$leader" "echo '$(findmnt -no MAJ:MIN -T "$data") wiops=max' > $cg/io.max" ;;
    far-leader) sidecar "$leader" "tc qdisc del dev eth0 root" ;;
  esac
}

# The fault scenarios: open-loop load on the followers, fault, heal, recovery.
faulted() {
  local system=$1 scenario=$2 out="$results/$1-$2"
  # One time zero for the generator and the injector (containers share the host clock),
  # far enough ahead for the generator's container to start.
  local t0=$(($(date +%s%3N) + 5000))
  docker run --rm --name qpx-gen --network "$net" --cpuset-cpus "$gen_cpus" -v "$results:/out" "$image" \
    kv-bench --protocol etcd --targets "${followers[0]}:2379,${followers[1]}:2379" --rate "$rate" \
    --gets 0 --secs "$secs" --conns 16 --start-at "$t0" --fault-at "$fault" --timeline 250 \
    --samples "/out/$1-$2.csv" > "$out.txt" &
  local gen=$!
  sleep_until $((t0 + fault * 1000))
  inject "$scenario"
  sleep_until $((t0 + heal * 1000))
  heal "$scenario"
  wait $gen
  head -2 "$out.txt" | sed 's/^/    /'
}

# A read-heavy open-loop load (90% linearizable ranges, 10% puts) on all three members.
mixed() {
  docker run --rm --name qpx-gen --network "$net" --cpuset-cpus "$gen_cpus" "$image" \
    kv-bench --protocol etcd --targets "${endpoints}" --rate "$mixed_rate" --gets 0.9 \
    --secs "$secs" --conns 16 > "$results/$1-mixed.txt"
  head -1 "$results/$1-mixed.txt" | sed 's/^/    /'
}

# Steady state with etcd's benchmark tool, against all three members.
steady() {
  local out="$results/$1-steady.txt" bench=(docker run --rm --network "$net" --cpuset-cpus "$gen_cpus" "$image"
    benchmark --endpoints="$endpoints" --conns 16 --clients 256)
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

for system in quepaxa etcd etcd-tuned; do
  for scenario in "${scenarios[@]}"; do
    # Timeouts do not matter without faults; the tuned etcd runs only the faults.
    [ "$system" = etcd-tuned ] && { [ "$scenario" = steady ] || [ "$scenario" = mixed ]; } && continue
    echo "$system: $scenario"
    case $system in
      quepaxa) start_quepaxa ;;
      etcd) start_etcd 100 1000 ;;
      etcd-tuned) start_etcd 10 100 ;;
    esac
    ready "$system"
    case $scenario in
      steady) steady "$system" ;;
      mixed) mixed "$system" ;;
      *) faulted "$system" "$scenario" ;;
    esac
    stop
  done
done
"$here/summarize.sh" "$results" > "$results/summary.md"
echo "results: $results"
cat "$results/summary.md"

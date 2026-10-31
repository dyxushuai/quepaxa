#!/usr/bin/env bash
# Compares a five-replica kv-tokio cluster (QuePaxa) with a five-member etcd cluster across
# five emulated regions on this host: every replica-to-replica link is delayed by the round
# trip between their regions (tc netem, per destination), each region's clients write to
# their local replica, and faults hit the leader's region. See README.md.
#
# Usage: wan.sh [--smoke] [scenario...]
#   scenarios: calm crash partition slow jitter (default: all)
#   --smoke    short single runs, to check the harness works
# Environment: RATE (total writes/s, default 1000), REPEATS (runs per case, default 3),
#   REPLICA_CPUS ("0-1 2-3 4-5 6-7 8-9"), GEN_CPUS (10-15), SLOW_MS (one-way delay added to
#   the leader's links, default 100), JITTER_MS (default 50) and LOSS (percent, default 1)
#   for the jitter scenario.
# Needs Docker and jq. Results go to target/bench-etcd/wan-<time>/.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)

smoke=0
scenarios=()
for a in "$@"; do
  case $a in
    --smoke) smoke=1 ;;
    calm | crash | partition | slow | jitter) scenarios+=("$a") ;;
    *) echo "unknown argument $a" >&2; exit 2 ;;
  esac
done
[ ${#scenarios[@]} -gt 0 ] || scenarios=(calm crash partition slow jitter)

if [ $smoke = 1 ]; then
  secs=8 fault=3 heal=6 repeats=1
else
  secs=40 fault=10 heal=25 repeats=${REPEATS:-3}
fi
rate=${RATE:-1000}
read -r -a replica_cpus <<< "${REPLICA_CPUS:-0-1 2-3 4-5 6-7 8-9}"
gen_cpus=${GEN_CPUS:-10-15}
slow_ms=${SLOW_MS:-100}
jitter_ms=${JITTER_MS:-50}
loss=${LOSS:-1}

# Round trips in ms between rough public-cloud regions, the same matrix as
# crates/quepaxa-sim/examples/wan_leaders.rs.
regions=(us-east us-west eu-west ap-ne sa-east)
rtt=(
  "0 70 80 180 120"
  "70 0 140 110 170"
  "80 140 0 230 190"
  "180 110 230 0 300"
  "120 170 190 300 0"
)
n=${#regions[@]}
rtt_of() { local row; read -r -a row <<< "${rtt[$1]}"; echo "${row[$2]}"; }
# A majority is three of five: a leader waits for its second-nearest peer, the third
# smallest entry of its row (counting itself).
rank_of() { tr ' ' '\n' <<< "${rtt[$1]}" | sort -n | sed -n 3p; }
by_rank=$(for i in $(seq 0 $((n - 1))); do echo "$(rank_of "$i") $i"; done | sort -n -k1,1 -k2,2 | cut -d' ' -f2)
best=$(head -1 <<< "$by_rank")
far=$(tail -1 <<< "$by_rank")
max_rtt=$(tr ' ' '\n' <<< "${rtt[*]}" | sort -n | tail -1)
# Leaders in order: the pinned region first, then the rest by rank.
leader_order() { { echo "$1"; grep -vx "$1" <<< "$by_rank"; } | paste -sd,; }

net=qpx-wan
prefix=172.31.251
replica_ip() { echo "$prefix.$((10 + $1))"; }
gen_ip() { echo "$prefix.$((20 + $1))"; }
results=$root/target/bench-etcd/wan-$(date +%Y%m%d-%H%M%S)
data=$root/target/bench-etcd/data
mkdir -p "$results" "$data"
# shellcheck source-path=SCRIPTDIR source=lib.sh
source "$here/lib.sh"

replicas=()
for i in $(seq 0 $((n - 1))); do replicas+=("qpx-r$i"); done
cleanup() {
  docker rm -f "${replicas[@]}" qpx-g0 qpx-g1 qpx-g2 qpx-g3 qpx-g4 >/dev/null 2>&1 || true
  wipe_data 2>/dev/null || true
}
trap 'cleanup; docker network rm "$net" >/dev/null 2>&1 || true' EXIT

build_images
docker network inspect "$net" >/dev/null 2>&1 || docker network create --subnet "$prefix.0/24" "$net" >/dev/null
{
  describe_host
  echo "regions: ${regions[*]}; far: ${regions[$far]}, best: ${regions[$best]}"
  for i in $(seq 0 $((n - 1))); do echo "rtt ${regions[$i]}: ${rtt[$i]}"; done
  echo "rate: $rate/s in total, run ${secs}s, fault at ${fault}s, heal at ${heal}s, repeats $repeats, smoke=$smoke"
  echo "slow: +${slow_ms}ms; jitter: ${jitter_ms}ms, ${loss}% loss"
  echo "cpus: replicas ${replica_cpus[*]}, generators $gen_cpus"
  echo "timers: quepaxa retransmit 400ms, hedge_max 3000ms; etcd 100/1000ms; etcd-tuned $max_rtt/$((10 * max_rtt))ms"
} > "$results/env.txt"
cat "$results/env.txt"

start_quepaxa() {
  local members="" i
  for i in $(seq 0 $((n - 1))); do members+="${members:+,}$i=$(replica_ip "$i"):7000"; done
  for i in $(seq 0 $((n - 1))); do
    docker run -d --name "qpx-r$i" --network "$net" --ip "$(replica_ip "$i")" --cpuset-cpus "${replica_cpus[$i]}" \
      -v "$data/r$i:/data" "$image" \
      kv-tokio --id "$i" --members "$members" --client 0.0.0.0:7100 --etcd 0.0.0.0:2379 --data /data \
      --leader-order "$(leader_order "$1")" --retransmit-ms 400 --hedge-max-ms 3000 >/dev/null
  done
}

# start_etcd <heartbeat ms> <election ms>
start_etcd() {
  local cluster="" i
  for i in $(seq 0 $((n - 1))); do cluster+="${cluster:+,}r$i=http://$(replica_ip "$i"):2380"; done
  for i in $(seq 0 $((n - 1))); do
    docker run -d --name "qpx-r$i" --network "$net" --ip "$(replica_ip "$i")" --cpuset-cpus "${replica_cpus[$i]}" \
      -v "$data/r$i:/data" "$etcd_image" \
      etcd --name "r$i" --data-dir /data \
      --listen-peer-urls http://0.0.0.0:2380 --initial-advertise-peer-urls "http://$(replica_ip "$i"):2380" \
      --listen-client-urls http://0.0.0.0:2379 --advertise-client-urls "http://$(replica_ip "$i"):2379" \
      --initial-cluster "$cluster" --initial-cluster-state new \
      --heartbeat-interval "$1" --election-timeout "$2" >/dev/null
  done
}

endpoints() {
  local i out=""
  for i in $(seq 0 $((n - 1))); do out+="${out:+,}$(replica_ip "$i"):2379"; done
  echo "$out"
}

# netem_on <replica> <peer> <netem args>: the delay on the replica's link to that peer.
netem_cmd() {
  local c=$((10 + $2))
  echo "tc qdisc del dev eth0 parent 1:$c 2>/dev/null; tc qdisc add dev eth0 parent 1:$c handle $c: netem $3 limit 100000"
}

# Every replica's egress: one HTB class per peer with half the round trip, everything else
# (the local generator) through an undelayed default class.
shape_links() {
  local i j cmd
  for i in $(seq 0 $((n - 1))); do
    cmd="tc qdisc add dev eth0 root handle 1: htb default 99 && tc class add dev eth0 parent 1: classid 1:99 htb rate 10gbit quantum 60000"
    for j in $(seq 0 $((n - 1))); do
      [ "$i" = "$j" ] && continue
      cmd+=" && tc class add dev eth0 parent 1: classid 1:$((10 + j)) htb rate 10gbit quantum 60000"
      cmd+=" && $(netem_cmd "$i" "$j" "delay $(($(rtt_of "$i" "$j") / 2))ms")"
      cmd+=" && tc filter add dev eth0 parent 1: protocol ip prio 1 u32 match ip dst $(replica_ip "$j")/32 flowid 1:$((10 + j))"
    done
    sidecar "qpx-r$i" "$cmd"
  done
}

# rtt_ms <pinger container or "gen:<i>"> <ip>: mean round trip over a few pings.
rtt_ms() {
  local out
  if [[ $1 == gen:* ]]; then
    out=$(docker run --rm --network "$net" --ip "$(gen_ip "${1#gen:}")" "$image" ping -c 5 -i 0.2 -q "$2")
  else
    out=$(sidecar "$1" "ping -c 5 -i 0.2 -q $2")
  fi
  sed -n 's|.*= [0-9.]*/\([0-9.]*\)/.*|\1|p' <<< "$out"
}

# Aborts unless every link from <replicas...> matches the matrix (10% or 5 ms).
check_links() {
  local i j want got
  for i in "$@"; do
    for j in $(seq 0 $((n - 1))); do
      [ "$i" = "$j" ] && continue
      want=$(rtt_of "$i" "$j")
      got=$(rtt_ms "qpx-r$i" "$(replica_ip "$j")")
      awk -v w="$want" -v g="$got" 'BEGIN { t = w / 10; if (t < 5) t = 5; d = g - w; if (d < 0) d = -d; exit !(g != "" && d <= t) }' ||
        { echo "link ${regions[$i]} -> ${regions[$j]}: ${got:-no reply} ms, want $want ms" >&2; exit 1; }
    done
  done
}

check_generators() {
  local i got
  for i in $(seq 0 $((n - 1))); do
    got=$(rtt_ms "gen:$i" "$(replica_ip "$i")")
    awk -v g="$got" 'BEGIN { exit !(g != "" && g < 1) }' ||
      { echo "generator ${regions[$i]} -> its replica: ${got:-no reply} ms, want under 1 ms" >&2; exit 1; }
  done
}

# The fast-path decisions a QuePaxa replica's proposers reached, from its STATS line.
fast_path() {
  docker run --rm --network "$net" "$image" bash -c \
    "exec 3<>/dev/tcp/$(replica_ip "$1")/7100 && echo STATS >&3 && head -1 <&3" 2>/dev/null |
    grep -o 'fast_path_decisions: [0-9]*' | grep -o '[0-9]*$' || echo 0
}

# The region leading now: for QuePaxa the replica whose fast path moves over a second
# (writes must be flowing), for etcd what it reports.
leader_now() {
  local i
  if [ "$1" = quepaxa ]; then
    local before=() best_i=- best_d=0 d
    for i in $(seq 0 $((n - 1))); do before[i]=$(fast_path "$i"); done
    sleep 1
    for i in $(seq 0 $((n - 1))); do
      d=$(($(fast_path "$i") - before[i]))
      [ "$d" -gt "$best_d" ] && best_d=$d best_i=$i
    done
    [ "$best_i" = - ] && echo none || echo "${regions[$best_i]}"
  else
    local st ip live="" eps=""
    for i in $(seq 0 $((n - 1))); do
      [ "$(docker inspect -f '{{.State.Running}}' "qpx-r$i" 2>/dev/null)" = true ] || continue
      live=${live:-$i}
      eps+="${eps:+,}$(replica_ip "$i"):2379"
    done
    st=$(docker exec "qpx-r$live" etcdctl --endpoints="$eps" endpoint status -w json 2>/dev/null || true)
    ip=$(jq -r '.[] | select(.Status.header.member_id == .Status.leader) | .Endpoint' <<< "${st:-[]}" |
      head -1 | sed 's|http://||; s|:2379||')
    for i in $(seq 0 $((n - 1))); do [ "$(replica_ip "$i")" = "$ip" ] && { echo "${regions[$i]}"; return; }; done
    echo none
  fi
}

# Waits for the cluster to take writes, then puts the leader in region <pinned>.
ready() {
  local system=$1 pinned=$2 deadline=$((SECONDS + 120)) hex leader_ep st
  until docker run --rm --network "$net" "$image" benchmark --endpoints="$(endpoints)" \
    put --total 10 >/dev/null 2>&1; do
    [ $SECONDS -lt $deadline ] || { echo "cluster did not come up" >&2; exit 1; }
    sleep 1
  done
  if [ "$system" != quepaxa ]; then
    for _ in $(seq 10); do
      [ "$(leader_now "$system")" = "${regions[$pinned]}" ] && break
      st=$(docker exec qpx-r0 etcdctl --endpoints="$(endpoints)" endpoint status -w json)
      leader_ep=$(jq -r '.[] | select(.Status.header.member_id == .Status.leader) | .Endpoint' <<< "$st" | head -1)
      hex=$(docker exec qpx-r0 etcdctl --endpoints="$(replica_ip "$pinned"):2379" endpoint status -w simple |
        cut -d, -f2 | xargs)
      docker exec qpx-r0 etcdctl --endpoints="$leader_ep" move-leader "$hex" >/dev/null 2>&1 || true
      sleep 2
    done
  else
    # Writes flowing, so the leader shows in the fast path.
    docker run -d --rm --name qpx-g0 --network "$net" "$image" benchmark --endpoints="$(endpoints)" \
      put --total 2000 --rate 500 >/dev/null
    sleep 1
  fi
  local now
  now=$(leader_now "$system")
  docker rm -f qpx-g0 >/dev/null 2>&1 || true
  [ "$now" = "${regions[$pinned]}" ] || { echo "$system leads from $now, not ${regions[$pinned]}" >&2; exit 1; }
  echo "  leader: $now"
}

peers_of() {
  local j
  for j in $(seq 0 $((n - 1))); do [ "$j" = "$1" ] || echo "$j"; done
}

# Sum of the TCP reordering events seen by every replica.
reorders() {
  local i total=0 c
  for i in $(seq 0 $((n - 1))); do
    c=$(sidecar "qpx-r$i" "nstat -asz TcpExtTCPSACKReorder TcpExtTCPTSReorder TcpExtTCPRenoReorder" |
      awk '/Reorder/ { s += $2 } END { print s + 0 }')
    total=$((total + c))
  done
  echo "$total"
}

inject() {
  local l=$2 j cmd=""
  case $1 in
    crash) docker kill -s KILL "qpx-r$l" >/dev/null ;;
    partition)
      for j in $(peers_of "$l"); do
        cmd+="iptables -A INPUT -s $(replica_ip "$j") -j DROP; iptables -A OUTPUT -d $(replica_ip "$j") -j DROP; "
      done
      sidecar "qpx-r$l" "$cmd" ;;
    slow)
      for j in $(peers_of "$l"); do
        cmd+="$(netem_cmd "$l" "$j" "delay $(($(rtt_of "$l" "$j") / 2 + slow_ms))ms"); "
      done
      sidecar "qpx-r$l" "$cmd" ;;
    jitter)
      # With a rate set, netem never sends a packet before the one queued ahead of it, so
      # jitter does not reorder (a FIFO child does not stop that).
      for j in $(peers_of "$l"); do
        cmd+="$(netem_cmd "$l" "$j" "delay $(($(rtt_of "$l" "$j") / 2))ms ${jitter_ms}ms distribution normal loss ${loss}% rate 10gbit"); "
      done
      sidecar "qpx-r$l" "$cmd" ;;
  esac
}

heal() {
  local l=$2 j cmd=""
  case $1 in
    partition) sidecar "qpx-r$l" "iptables -F" ;;
    slow | jitter)
      for j in $(peers_of "$l"); do cmd+="$(netem_cmd "$l" "$j" "delay $(($(rtt_of "$l" "$j") / 2))ms"); "; done
      sidecar "qpx-r$l" "$cmd" ;;
  esac
}

# One run: a generator per region on its local replica, the fault on the leader's region.
run_case() {
  local system=$1 scenario=$2 pinned=$3 name=$4 run=$5 i pids=() before
  local t0=$(($(date +%s%3N) + 5000)) per=$((rate / n))
  for i in $(seq 0 $((n - 1))); do
    docker run --rm --name "qpx-g$i" --network "$net" --ip "$(gen_ip "$i")" --cpuset-cpus "$gen_cpus" \
      -v "$results:/out" "$image" \
      kv-bench --protocol etcd --targets "$(replica_ip "$i"):2379" --rate "$per" --gets 0 --secs "$secs" \
      --conns 4 --start-at "$t0" --fault-at "$fault" --timeline 250 --seed $((run * 10 + i + 1)) \
      --samples "/out/$name-r$i.csv" > "$results/$name-r$i.txt" &
    pids+=($!)
  done
  [ "$scenario" = jitter ] && before=$(reorders)
  sleep_until $((t0 + fault * 1000))
  inject "$scenario" "$pinned"
  sleep_until $((t0 + heal * 1000))
  heal "$scenario" "$pinned"
  sleep_until $((t0 + (heal + (secs - heal) / 2) * 1000))
  {
    echo "leader_after=$(leader_now "$system")"
    [ "$scenario" = jitter ] && echo "reorders=$(($(reorders) - before))"
  } > "$results/$name-leader.txt"
  for i in "${pids[@]}"; do wait "$i"; done
  case $scenario in slow | jitter) check_links "$pinned" ;; esac
  awk '/^offered/ { for (i = 1; i <= NF; i++) if ($i ~ /^p99=/) p = p " " $i } END { print "    p99 by region:" p }' \
    "$results/$name"-r?.txt
}

stop() {
  docker rm -f "${replicas[@]}" >/dev/null 2>&1 || true
  wipe_data
}

for system in quepaxa etcd etcd-tuned; do
  for scenario in "${scenarios[@]}"; do
    placements=(far)
    [ "$scenario" = calm ] && placements+=(best)
    for placement in "${placements[@]}"; do
      [ "$placement" = far ] && pinned=$far || pinned=$best
      label=$scenario
      [ "$placement" = best ] && label=$scenario-best
      for run in $(seq 1 "$repeats"); do
        echo "$system: $label, run $run"
        case $system in
          quepaxa) start_quepaxa "$pinned" ;;
          etcd) start_etcd 100 1000 ;;
          etcd-tuned) start_etcd "$max_rtt" $((10 * max_rtt)) ;;
        esac
        shape_links
        check_links $(seq 0 $((n - 1)))
        check_generators
        ready "$system" "$pinned"
        run_case "$system" "$scenario" "$pinned" "$system-$label-$run" "$run"
        stop
      done
    done
  done
done
"$here/summarize-wan.sh" "$results" > "$results/summary.md"
echo "results: $results"
cat "$results/summary.md"

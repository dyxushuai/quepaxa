#!/usr/bin/env bash
# Compares a five-replica kv-tokio cluster (QuePaxa) with a five-member etcd cluster across
# five emulated regions on this host: every replica-to-replica link is delayed by the round
# trip between their regions (tc netem, per destination), each region's clients write to
# their local replica, and faults hit the leader's region. See README.md.
#
# Usage: wan.sh [--smoke] [scenario...]
#   scenarios: calm crash partition slow jitter (default: all), and the leader-policy
#     scenarios (QuePaxa only, never by default): convergence (leader starts far away),
#     heterogeneous (no WAN delay, the first leader's container on a CPU quota), shift
#     (writes move from the regions near us-east to those near ap-ne for good), burst (far
#     regions write five times as much for 10 s)
#   --smoke    short single runs, to check the harness works
# Environment: RATE (total writes/s, default 1000), REPEATS (runs per case, default 3),
#   REPLICA_CPUS ("0-1 2-3 4-5 6-7 8-9"), GEN_CPUS (10-15), SLOW_MS (one-way delay added to
#   the leader's links, default 100), JITTER_MS (default 50) and LOSS (percent, default 1)
#   for the jitter scenario; POLICY (kv-tokio flags of the leader policy under test,
#   default "--placement on --placement-periods 30000,120000") for the policy scenarios,
#   and for the others too with POLICY_EVERYWHERE=1; SLOW_CPUS (the heterogeneous
#   scenario's quota, default 0.5); SLOW_REGION (the region whose container gets that quota,
#   default the first leader's); SYSTEMS (default "quepaxa etcd etcd-tuned").
# Needs Docker and jq. Results go to target/bench-etcd/wan-<time>/.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)

smoke=0
scenarios=()
for a in "$@"; do
  case $a in
    --smoke) smoke=1 ;;
    calm | crash | partition | slow | jitter | convergence | heterogeneous | shift | burst) scenarios+=("$a") ;;
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
policy=${POLICY:---placement on --placement-periods 30000,120000}
slow_cpus=${SLOW_CPUS:-0.5}
slow_region=${SLOW_REGION:-}
# The policy scenarios: run length, when the load changes, and the last stretch whose
# latency is reported (after the policy has settled), in seconds.
policy_case() { case $1 in convergence | heterogeneous | shift | burst) return 0 ;; esac; return 1; }
if [ $smoke = 1 ]; then
  policy_secs=30 change_at=15 change_for=5 settled_from=20
else
  policy_secs=300 change_at=150 change_for=10 settled_from=240
fi
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
[ -z "$slow_region" ] || printf '%s\n' "${regions[@]}" | grep -qx -- "$slow_region" ||
  { echo "unknown SLOW_REGION $slow_region" >&2; exit 2; }
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

replicas=() gens=()
for i in $(seq 0 $((n - 1))); do replicas+=("qpx-r$i") gens+=("qpx-g$i"); done
cleanup() {
  docker rm -f "${replicas[@]}" "${gens[@]}" qpx-burst >/dev/null 2>&1 || true
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
  echo "policy: $policy (everywhere: ${POLICY_EVERYWHERE:-0}); policy runs ${policy_secs}s, change at ${change_at}s for ${change_for}s, settled from ${settled_from}s; heterogeneous quota $slow_cpus CPUs on ${slow_region:-the first leader}"
  echo "cpus: replicas ${replica_cpus[*]}, generators $gen_cpus"
  echo "timers: quepaxa retransmit 400ms, hedge_max 3000ms; etcd 100/1000ms; etcd-tuned $max_rtt/$((10 * max_rtt))ms"
} > "$results/env.txt"
cat "$results/env.txt"

# start_quepaxa <pinned> <scenario>: the pinned region leads first; the policy flags apply
# to the policy scenarios (and to all with POLICY_EVERYWHERE=1); the heterogeneous
# scenario puts the first leader's container (or SLOW_REGION's) on a CPU quota.
start_quepaxa() {
  local members="" i flags=() quota slow=$1
  for i in $(seq 0 $((n - 1))); do members+="${members:+,}$i=$(replica_ip "$i"):7000"; done
  if policy_case "$2" || [ "${POLICY_EVERYWHERE:-0}" = 1 ]; then read -r -a flags <<< "$policy"; fi
  if [ -n "$slow_region" ]; then
    for i in $(seq 0 $((n - 1))); do [ "${regions[$i]}" = "$slow_region" ] && slow=$i; done
  fi
  for i in $(seq 0 $((n - 1))); do
    quota=()
    [ "$2" = heterogeneous ] && [ "$i" = "$slow" ] && quota=(--cpus "$slow_cpus")
    docker run -d --name "qpx-r$i" --network "$net" --ip "$(replica_ip "$i")" --cpuset-cpus "${replica_cpus[$i]}" \
      "${quota[@]}" -v "$data/r$i:/data" "$image" \
      kv-tokio --id "$i" --members "$members" --client 0.0.0.0:7100 --etcd 0.0.0.0:2379 --data /data \
      --leader-order "$(leader_order "$1")" --retransmit-ms 400 --hedge-max-ms 3000 "${flags[@]}" >/dev/null
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

running() { [ "$(docker inspect -f '{{.State.Running}}' "qpx-r$1" 2>/dev/null)" = true ]; }

# The client endpoints of the replicas still running.
endpoints() {
  local i out=""
  for i in $(seq 0 $((n - 1))); do running "$i" && out+="${out:+,}$(replica_ip "$i"):2379"; done
  echo "$out"
}

# etcd's leader endpoint, as its running members report it.
etcd_leader() {
  local i
  for i in $(seq 0 $((n - 1))); do running "$i" && break; done
  docker exec "qpx-r$i" etcdctl --endpoints="$(endpoints)" endpoint status -w json 2>/dev/null |
    jq -r '.[] | select(.Status.header.member_id == .Status.leader) | .Endpoint' | head -1
}

# netem_cmd <add|change> <replica> <peer> [extra ms] [netem args]: the replica's link to that
# peer, delayed by half their round trip plus the extra. Every link has a rate, far above
# the load: with one, netem never sends a packet before the one queued ahead of it, so
# jitter does not reorder (and holds a packet behind a slower one ahead instead). Faults
# change a link in place; replacing its qdisc would drop the packets queued in it.
netem_cmd() {
  local c=$((10 + $3)) ms=$(($(rtt_of "$2" "$3") / 2 + ${4:-0}))
  echo "tc qdisc $1 dev eth0 parent 1:$c handle $c: netem delay ${ms}ms ${5:-} rate 10gbit limit 100000"
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
      cmd+=" && $(netem_cmd add "$i" "$j")"
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

# The leader order in force a QuePaxa replica reports in its STATS line: ids, leader first.
leaders_of() {
  docker run --rm --network "$net" "$image" bash -c \
    "exec 3<>/dev/tcp/$(replica_ip "$1")/7100 && echo STATS >&3 && head -1 <&3" 2>/dev/null |
    grep -o 'LEADERS [0-9,]*' | cut -d' ' -f2
}

# The epoch in force any QuePaxa replica reports in its STATS line.
epoch_now() {
  local i e
  for i in $(seq 0 $((n - 1))); do
    e=$(docker run --rm --network "$net" "$image" bash -c \
      "exec 3<>/dev/tcp/$(replica_ip "$i")/7100 && echo STATS >&3 && head -1 <&3" 2>/dev/null |
      grep -o 'epoch: [0-9]*' | cut -d' ' -f2)
    [ -n "$e" ] && { echo "$e"; return; }
  done
  echo -
}

# The region leading now: the first replica of the order in force any QuePaxa replica
# reports, or what etcd reports.
leader_now() {
  local i
  if [ "$1" = quepaxa ]; then
    local order
    for i in $(seq 0 $((n - 1))); do
      order=$(leaders_of "$i")
      [ -n "$order" ] && { echo "${regions[${order%%,*}]}"; return; }
    done
    echo none
  else
    local ip
    ip=$(etcd_leader | sed 's|http://||; s|:2379||')
    for i in $(seq 0 $((n - 1))); do [ "$(replica_ip "$i")" = "$ip" ] && { echo "${regions[$i]}"; return; }; done
    echo none
  fi
}

# Waits for the cluster to take writes, then puts the leader in region <pinned>.
ready() {
  local system=$1 pinned=$2 deadline=$((SECONDS + 120)) hex
  until docker run --rm --network "$net" "$image" benchmark --endpoints="$(endpoints)" \
    put --total 10 >/dev/null 2>&1; do
    [ $SECONDS -lt $deadline ] || { echo "cluster did not come up" >&2; exit 1; }
    sleep 1
  done
  if [ "$system" != quepaxa ]; then
    for _ in $(seq 10); do
      [ "$(leader_now "$system")" = "${regions[$pinned]}" ] && break
      hex=$(docker exec qpx-r0 etcdctl --endpoints="$(replica_ip "$pinned"):2379" endpoint status -w simple |
        cut -d, -f2 | xargs)
      docker exec qpx-r0 etcdctl --endpoints="$(etcd_leader)" move-leader "$hex" >/dev/null 2>&1 || true
      sleep 2
    done
  fi
  local now
  now=$(leader_now "$system")
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
        cmd+="$(netem_cmd change "$l" "$j" "$slow_ms"); "
      done
      sidecar "qpx-r$l" "$cmd" ;;
    jitter)
      for j in $(peers_of "$l"); do
        cmd+="$(netem_cmd change "$l" "$j" 0 "${jitter_ms}ms distribution normal loss ${loss}%"); "
      done
      sidecar "qpx-r$l" "$cmd" ;;
  esac
}

heal() {
  local l=$2 j cmd=""
  case $1 in
    partition) sidecar "qpx-r$l" "iptables -F" ;;
    slow | jitter)
      for j in $(peers_of "$l"); do cmd+="$(netem_cmd change "$l" "$j"); "; done
      sidecar "qpx-r$l" "$cmd" ;;
  esac
}

# region_load <scenario> <region>: kv-bench rate flags for that region's generator.
region_load() {
  local per=$((rate / n)) region=${regions[$2]}
  case $1 in
    shift)
      # Writes come from the regions near us-east, then from those near ap-ne, for good.
      case $region in
        us-east | eu-west | sa-east) echo "--rate $((rate / 3)) --rate-at $change_at:0" ;;
        *) echo "--rate 0 --rate-at $change_at:$((rate / 2))" ;;
      esac ;;
    burst)
      case $region in
        ap-ne | sa-east) echo "--rate $per --rate-at $change_at:$((5 * per)),$((change_at + change_for)):$per" ;;
        *) echo "--rate $per" ;;
      esac ;;
    *) echo "--rate $per" ;;
  esac
}

# From t0 ($1), every 5 s until the file named by $2 exists, appends to $3 "<seconds since
# t0> <leading region>".
sample_leaders() {
  local t0=$1 stop=$2 out=$3
  sleep_until "$t0"
  until [ -e "$stop" ]; do
    # The epoch tells a decided order change (it advances) from a rotation (it does not).
    echo "$((($(date +%s%3N) - t0) / 1000)) $(leader_now quepaxa) $(epoch_now)" >> "$out"
    sleep 5
  done
}

# One run: a generator per region on its local replica, the fault on the leader's region.
run_case() {
  local system=$1 scenario=$2 pinned=$3 name=$4 run=$5 i pids=() before load sampler=""
  # The policy scenarios report the longest stall after their load changes instead.
  local run_secs=$secs fault_at=$fault
  policy_case "$scenario" && run_secs=$policy_secs fault_at=$change_at
  # Probes (containers of their own) run before and after the load, never during it; only
  # the policy scenarios sample the leader meanwhile, from the generators' CPUs.
  [ "$scenario" = jitter ] && before=$(reorders)
  local t0=$(($(date +%s%3N) + 5000))
  for i in $(seq 0 $((n - 1))); do
    read -r -a load <<< "$(region_load "$scenario" "$i")"
    docker run --rm --name "qpx-g$i" --network "$net" --ip "$(gen_ip "$i")" --cpuset-cpus "$gen_cpus" \
      -v "$results:/out" "$image" \
      kv-bench --protocol etcd --targets "$(replica_ip "$i"):2379" "${load[@]}" --gets 0 --secs "$run_secs" \
      --conns 4 --start-at "$t0" --fault-at "$fault_at" --timeline 250 --seed $((run * 10 + i + 1)) \
      --samples "/out/$name-r$i.csv" > "$results/$name-r$i.txt" &
    pids+=($!)
  done
  if policy_case "$scenario"; then
    rm -f "$results/$name.stop"
    sample_leaders "$t0" "$results/$name.stop" "$results/$name-leaders.txt" &
    sampler=$!
  else
    sleep_until $((t0 + fault * 1000))
    inject "$scenario" "$pinned"
    sleep_until $((t0 + heal * 1000))
    heal "$scenario" "$pinned"
  fi
  for i in "${pids[@]}"; do wait "$i"; done
  if [ -n "$sampler" ]; then
    touch "$results/$name.stop"
    wait "$sampler"
    # Each replica's counters (placement moves, steps aside, epoch) say who moved the lead.
    for i in $(seq 0 $((n - 1))); do
      echo "${regions[$i]} $(docker run --rm --network "$net" "$image" bash -c \
        "exec 3<>/dev/tcp/$(replica_ip "$i")/7100 && echo STATS >&3 && head -1 <&3" 2>/dev/null)"
    done > "$results/$name-stats.txt"
    # Decided order changes, as each replica logged them; a run without any logs none (grep's
    # status 1), which is no failure.
    for i in $(seq 0 $((n - 1))); do
      docker logs "qpx-r$i" 2>&1 | { grep 'epoch' || [ $? = 1 ]; } | sed "s/^/${regions[$i]} /"
    done > "$results/$name-epochs.txt"
  fi
  {
    echo "leader_after=$(leader_now "$system")"
    [ "$scenario" = jitter ] && echo "reorders=$(($(reorders) - before))"
    if policy_case "$scenario"; then
      awk -v from="$settled_from" '
        $2 != last { if (NR > 1) { changes++; at = $1 } last = $2 }
        END { printf "leader_changes=%d\nlast_change_s=%d\nmeasure_from_s=%d\n", changes, at, from }' \
        "$results/$name-leaders.txt"
    fi
  } > "$results/$name-leader.txt"
  case $scenario in slow | jitter) check_links "$pinned" ;; esac
  awk '/^offered/ { for (i = 1; i <= NF; i++) if ($i ~ /^p99=/) p = p " " $i } END { print "    p99 by region:" p }' \
    "$results/$name"-r?.txt
}

stop() {
  docker rm -f "${replicas[@]}" >/dev/null 2>&1 || true
  wipe_data
}

read -r -a systems <<< "${SYSTEMS:-quepaxa etcd etcd-tuned}"
for system in "${systems[@]}"; do
  for scenario in "${scenarios[@]}"; do
    policy_case "$scenario" && [ "$system" != quepaxa ] && continue
    placements=(far)
    [ "$scenario" = calm ] && placements+=(best)
    # The write shift starts where the first writers are best served.
    [ "$scenario" = shift ] || [ "$scenario" = burst ] && placements=(best)
    for placement in "${placements[@]}"; do
      [ "$placement" = far ] && pinned=$far || pinned=$best
      label=$scenario
      [ "$placement" = best ] && label=$scenario-best
      for run in $(seq 1 "$repeats"); do
        echo "$system: $label, run $run"
        case $system in
          quepaxa) start_quepaxa "$pinned" "$scenario" ;;
          etcd) start_etcd 100 1000 ;;
          etcd-tuned) start_etcd "$max_rtt" $((10 * max_rtt)) ;;
        esac
        if [ "$scenario" != heterogeneous ]; then
          shape_links
          check_links $(seq 0 $((n - 1)))
        fi
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

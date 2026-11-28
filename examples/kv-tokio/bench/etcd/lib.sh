# shellcheck shell=bash disable=SC2154
# Helpers shared by run.sh and wan.sh; sourced, not run. The caller sets $root (repository
# root), $here (this directory) and $data (the data directory on the disk under test).

image=qpx-bench:local
openraft_image=qpx-openraft:local
etcd_image=gcr.io/etcd-development/etcd:v3.7.2@sha256:7c6c239825d00e3f6328a69caafd54be92063acf0c2ce78b8394699f52b75dc3

build_images() {
  echo "building $image"
  docker build -q -t "$image" -f "$here/Dockerfile" "$root" >/dev/null
  docker pull -q "$etcd_image" >/dev/null
}

build_openraft_image() {
  echo "building $openraft_image"
  docker build -q -t "$openraft_image" -f "$root/examples/kv-tokio/bench/openraft/Dockerfile" "$root" >/dev/null
}

# The host, versions and measured fsync latency, for env.txt.
describe_host() {
  echo "date: $(date -u +%FT%TZ)"
  echo "commit: $(git -C "$root" rev-parse HEAD)"
  echo "kernel: $(uname -sr)"
  echo "cpu: $(nproc) x $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | xargs)"
  echo "load: $(cut -d' ' -f1-3 /proc/loadavg)"
  echo "docker: $(docker version --format '{{.Server.Version}}')"
  echo "image: $(docker image inspect --format '{{.Id}}' "$image")"
  if docker image inspect "$openraft_image" >/dev/null 2>&1; then
    echo "openraft image: $(docker image inspect --format '{{.Id}}' "$openraft_image")"
  fi
  echo "etcd image: $etcd_image"
  echo "data filesystem: $(df -T "$data" | awk 'NR==2 {print $1, $2}')"
  # 200 appends of 512 bytes, each fsynced, in a container on the data disk.
  local t
  t=$(docker run --rm -v "$data:/d" "$image" sh -c \
    'start=$(date +%s%N); for i in $(seq 200); do dd if=/dev/zero of=/d/fsync bs=512 count=1 oflag=append conv=notrunc,fsync 2>/dev/null; done; echo $(( ($(date +%s%N) - start) / 200000 ))')
  echo "fsync: ${t}us per 512-byte append (mean of 200)"
}

# Files written in the containers belong to root: remove them from a container.
wipe_data() {
  docker run --rm -v "$data:/d" "$image" sh -c 'rm -rf /d/*' >/dev/null
}

# sidecar <container> <command>: runs in that container's network namespace, with the
# host's cgroups.
sidecar() {
  docker run --rm --privileged --network "container:$1" -v /sys/fs/cgroup:/sys/fs/cgroup "$image" sh -c "$2"
}

cgroup_of() {
  local id cg
  id=$(docker inspect -f '{{.Id}}' "$1")
  cg=$(find /sys/fs/cgroup -maxdepth 3 -type d \( -name "docker-$id.scope" -o -name "$id" \) | head -1)
  # Without it the fault would silently not happen.
  [ -n "$cg" ] || { echo "no cgroup v2 directory for $1" >&2; exit 1; }
  echo "$cg"
}

# sleep_until <Unix ms>
sleep_until() {
  local ms=$(($1 - $(date +%s%3N)))
  [ "$ms" -le 0 ] || sleep "$((ms / 1000)).$(printf %03d $((ms % 1000)))"
}

# probe_disk <t0> <secs> <file> [cpuset]: from t0 for that long, once a second, appends
# 512 bytes with fsync to the replicas' data disk and writes "<second> <us>" to that file
# in $results: the wall time of one dd, process start included. A shared virtual disk can
# switch between fsync regimes that move every commit latency (docs/benchmarks.md).
probe_disk() {
  local cpus=${4:-${probe_cpus:-$gen_cpus}}
  # shellcheck disable=SC2016 # expanded in the container
  docker run --rm --name qpx-disk --cpuset-cpus "$cpus" -v "$data:/d" -v "$results:/out" "$image" bash -c '
    t0=$1 secs=$2 out=/out/$3 k=0
    now_us() { echo "${EPOCHREALTIME/[.,]/}"; }
    append() { dd if=/dev/zero of=/d/probe bs=512 count=1 oflag=append conv=notrunc,fsync 2>/dev/null; }
    # Unrecorded: the first append creates the file and takes several times longer.
    append
    while [ "$k" -lt "$secs" ]; do
      ms=$((t0 + k * 1000 - $(now_us) / 1000))
      [ "$ms" -le 0 ] || sleep "$((ms / 1000)).$(printf %03d $((ms % 1000)))"
      s=$(now_us)
      append
      e=$(now_us)
      echo "$k $((e - s))" >> "$out"
      # A stalled fsync skips the seconds it took instead of firing a burst after it.
      k=$(((e / 1000 - t0) / 1000 + 1))
    done' _ "$1" "$2" "$3"
}

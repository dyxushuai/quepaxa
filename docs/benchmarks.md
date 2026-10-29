# Benchmarks

Measurements of one machine and configuration. They are hypotheses about performance, not
production claims; rerun them on your own hardware before drawing conclusions.

## Environment (2026-10-13)

- 16 vCPU VM (`Genuine Intel(R) 0000`, Proxmox kernel 7.0.14-17-pve), local NVMe-backed disk.
- Rust 1.101 nightly toolchain for builds; `--release`.
- `quepaxa` at the commit that introduced this file; default `Config` except as noted.

## Core hot paths (`cargo bench -p quepaxa --bench replica`)

CPU only: messages are delivered and persists acknowledged in memory, no I/O.

| Benchmark | Time per op |
|---|---|
| `single_member_propose_to_commit` (propose → persist → commit, 1 replica) | 2.07 µs |
| `three_members_leader_propose_to_commit` (leader fast path, 3 replicas) | 5.63 µs |
| `reject_malformed_message` (decode and drop a bad frame) | 29 ns |

## End to end (`examples/kv-tokio/run-local.sh <rate> <secs> [delay-ms]`)

Three `kv-tokio` processes on loopback, each persisting with `fsync` to its own data
directory; `kv-bench` drives an open-loop Poisson workload in the Go reference
implementation's shape — 17-byte commands (1-byte operation, 8-byte key, 8-byte value),
50% `PUT` / 50% `GET` (linearizable, via read barriers), random keys from a 52-letter
alphabet — over 24 pipelined connections spread across the replicas, 5 s per point.
Requests unanswered 5 s after the run are counted separately; none occurred.

### On disk

Data under `target/kv-data` on the VM's ext4 volume (a Proxmox virtual disk; one 512-byte
append + `fsync` takes 1.1 ms at the median, 2.1 ms at p99). Measured 2026-10-15. The disk
is shared with other VMs: an occasional run shows a p99 several times higher; rerun before
trusting a single outlier.

| Offered rate | Achieved | Errors | p50 | p90 | p99 |
|---|---|---|---|---|---|
| 5,000/s | 4,974/s | 0 | 5.9 ms | 9.2 ms | 11.4 ms |
| 20,000/s | 20,096/s | 0 | 6.4 ms | 9.4 ms | 11.9 ms |
| 50,000/s | 49,953/s | 0 | 6.5 ms | 9.5 ms | 11.0 ms |
| 100,000/s | 99,829/s | 0 | 6.5 ms | 9.5 ms | 11.4 ms |
| 150,000/s | 149,849/s | 0 | 6.6 ms | 9.8 ms | 17.9 ms |
| 200,000/s | 199,915/s | 0 | 6.9 ms | 9.4 ms | 14.7 ms |
| 250,000/s | 250,027/s | 0 | 7.2 ms | 10.0 ms | 14.2 ms |

Latency is a few `fsync`s on the commit path and barely depends on load: each fsync
covers everything queued while the previous one ran (group commit), so throughput scales
without longer waits. Two changes (#50) halved it from 11.6 ms (p50 at 5k/s):

- **Commits are applied before their local `Decided` record is durable.** A decision is
  already durable at a majority of recorders, so a replica that crashes first learns it
  again; this takes one fsync off every replica's commit path (11.6 → 9.2 ms alone).
- **kv-tokio writes on its own thread.** The event loop no longer blocks in `fsync`:
  persists issued meanwhile share the next one (9.2 → 5.9 ms with the change above).

### In memory (CPU bound)

The same runs with the data directories on tmpfs, where `fsync` costs nothing: an upper
bound set by CPU and loopback I/O. Until 2026-10-15 `run-local.sh` put its data in
`mktemp -d`, which is tmpfs on this VM, so these were the numbers first published here as
"persisting with fsync"; they were not. `run-local.sh` now defaults to `target/kv-data`
(override with `KV_DATA_ROOT`) and warns on tmpfs. Measured 2026-10-14.

| Offered rate | Achieved | Errors | p50 | p90 | p99 |
|---|---|---|---|---|---|
| 5,000/s | 4,974/s | 0 | 0.29 ms | 0.58 ms | 0.95 ms |
| 20,000/s | 20,095/s | 0 | 0.33 ms | 0.50 ms | 1.0 ms |
| 50,000/s | 49,943/s | 0 | 0.42 ms | 0.63 ms | 1.8 ms |
| 100,000/s | 99,841/s | 0 | 0.58 ms | 0.92 ms | 14.0 ms |
| 150,000/s | 149,843/s | 0 | 0.72 ms | 1.2 ms | 26.5 ms |
| 200,000/s | 199,903/s | 43 (`Busy`) | 0.81 ms | 1.9 ms | 42.2 ms |
| 250,000/s | 240,314/s | 48,629 (`Busy`) | 1.1 ms | 752 ms | 2,253 ms |

Here saturation sets in near 200k operations/s, where `max_outstanding_proposals` (4096
per replica) pushes back with `Busy`. Before #50 it was near 100k/s (p90 30.7 ms, p99
124 ms at 100k/s). The changes, each found by measurement:

- **Peer and client I/O batching** (`perf`: 65% of CPU in Tokio workers, about half of
  it in the kernel, one or two syscalls per frame). Frames queued while a batch is being
  written now leave in one `write`; reads go through a 64 KiB buffer; client replies are
  flushed only before waiting on one that is not ready.
- **Snapshots off the event loop** (stall-fills and leader rotations clustered at every
  10,000th slot), then shared by shard and encoded on a background thread, with mimalloc
  for the cross-thread frees.
- **CRC-32C slicing-by-8**: `Record::encode` (dominated by the checksum) went from 6.7%
  to 1.8% of the replica thread's samples.

### Profiling

```sh
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --profile profiling -p kv-tokio
perf record -F 2999 -g --call-graph fp -p <kv-tokio pid> -- sleep 6   # while kv-bench runs
perf report --no-children --sort sym
```

`kv-bench` prints every replica's `Metrics` after a run (the `STATS` client command), so
retransmits, hedges, forwards and fast-path decisions are visible next to the latencies.

### Simulated WAN

`run-local.sh 5000 8 50` adds a 50 ms one-way `netem` delay to all loopback traffic,
including the client connections (so every client request also pays a 100 ms round trip):

| Offered rate | Achieved | p50 | p90 | p99 |
|---|---|---|---|---|
| 5,000/s | 4,926/s | 319 ms | 596 ms | 919 ms |

The median matches the path: client round trip (100 ms) + forward to the leader (50 ms) +
leader fast path (100 ms) + decision broadcast to the client's replica (50 ms), plus fsync.

## Simulated WAN placement (`cargo run --release -p quepaxa-sim --example wan_leaders`)

Five replicas in five regions (rough public-cloud round trips: us-east, us-west, eu-west,
ap-northeast, sa-east), fault-free, 20 s of about 100 writes/s spread over all replicas,
client latency from `propose` to `Committed`, 3 seeds, no snapshots (the simulator ships
them instantly). Each row swaps the initial leader (replica 0) with another region and
keeps the rest of the placement. Timers suit the WAN: `retransmit_interval` 400 ms (above the longest round
trip), `hedge_max` 3 s.

| Leader region | p50 | p90 | p99 |
|---|---|---|---|
| us-east | 181 ms | 279 ms | 299 ms |
| us-west | 241 ms | 302 ms | 324 ms |
| eu-west | 305 ms | 395 ms | 420 ms |
| ap-northeast | 389 ms | 508 ms | 535 ms |
| sa-east | 365 ms | 497 ms | 526 ms |

No hedges, no retransmits, every slot on the fast path: the leader's placement alone moves
client latency by about 2x (#28).

The timers matter as much. With the simulator's LAN timers (20 ms retransmit, 400 ms hedge
cap) the same runs retransmitted every request several times and, with a distant leader,
capped the hedge delay below the commit path, so followers hedged nearly every entry and
p99 reached 6-17 s. The library defaults (50 ms retransmit, 1 s hedge cap) are tuned for a
LAN: on a WAN set `retransmit_interval` above the round trip to the farthest member of a
majority, and `hedge_max` above the slowest client commit path.

## Against etcd

`examples/kv-tokio/bench/etcd/run.sh` (method, fairness rules and how to read each column:
[its README](../examples/kv-tokio/bench/etcd/README.md)). Three `kv-tokio` replicas and
three etcd v3.7.2 members, one system at a time, each node in its own container with
three cores, data on the VM's ext4 disk (one 512-byte append + `fsync`: 1.7 ms mean). Both
are driven through etcd's v3 KV gRPC API by the same clients, which `kv-tokio` serves with
`--etcd`; QuePaxa runs its default `Config`. Measured 2026-10-29 at commit `6dba864`; raw
results in `target/bench-etcd/20261029-210608/` of that run.

### Steady state

etcd's own `benchmark` tool (closed loop, 256 clients over 16 connections, all three
members):

| Workload | etcd req/s | etcd p50 / p99 | QuePaxa req/s | QuePaxa p50 / p99 |
|---|---|---|---|---|
| put, unlimited | 24,525 | 10.1 / 19.1 ms | 32,154 | 7.2 / 13.9 ms |
| put, 5,000/s | 4,998 | 8.1 / 15.5 ms | 4,998 | 5.9 / 11.2 ms |
| txn-put | 23,303 | 10.6 / 20.9 ms | 30,776 | 7.4 / 19.9 ms |
| range, linearizable | 99,409 | 1.6 / 12.4 ms | 33,916 | 6.9 / 11.8 ms |
| range, serializable | 146,246 | 0.9 / 6.7 ms | 159,978 | 0.8 / 5.1 ms |

Writes: QuePaxa commits in fewer `fsync`s on the path (a decision is applied before its
local record is durable), so 30% more throughput at lower latency. Linearizable reads are
QuePaxa's weak spot: etcd confirms leadership with a heartbeat round and reads locally
(ReadIndex), while `kv-tokio` puts every linearizable read through a read barrier, a slot
in the log, so it costs about what a write does: a third of etcd's throughput.

### Leader faults

Open-loop load (`kv-bench --protocol etcd`, 5,000 puts/s, Poisson arrivals) on the two
followers for 40 s; the fault starts at 10 s and is undone at 25 s (a crashed leader stays
down). Latency runs from each request's due time; percentiles cover successful requests
only, so a request etcd fails after its internal 7 s timeout is an error, not a 7 s sample.
etcd runs with its default timeouts (heartbeat 100 ms, election 1 s) and tuned for a fast
local network (10 ms, 100 ms).

| Fault | System | p99 | max | Errors | Longest gap | Degraded |
|---|---|---|---|---|---|---|
| crash (`SIGKILL`) | etcd | 856 ms | 1,645 ms | 3,245 | 1,612 ms | 1,750 ms |
| | etcd, tuned | 16 ms | 137 ms | 296 | 125 ms | 250 ms |
| | QuePaxa | 495 ms | 788 ms | 0 | 561 ms | 3,000 ms |
| partition (15 s) | etcd | 1,097 ms | 1,869 ms | 4,368 | 1,824 ms | 1,750 ms |
| | etcd, tuned | 17 ms | 182 ms | 429 | 170 ms | 250 ms |
| | QuePaxa | 624 ms | 1,042 ms | 0 | 405 ms | 1,000 ms |
| slow disk (50 write IOPS, 15 s) | etcd | 396 ms | 509 ms | 0 | 203 ms | 4,250 ms |
| | etcd, tuned | 243 ms | 401 ms | 914 | 119 ms | 500 ms |
| | QuePaxa | 12.5 ms | 96 ms | 0 | 48 ms | 250 ms |
| far leader (+50 ms, 15 s) | etcd | 203 ms | 218 ms | 0 | 56 ms | 250 ms |
| | etcd, tuned | 622 ms | 960 ms | 0 | 418 ms | 1,250 ms |
| | QuePaxa | 327 ms | 619 ms | 0 | 196 ms | 750 ms |

What the table says, and what it does not:

- **No request failed on QuePaxa** in any scenario. etcd answers clients with errors while
  it has no leader (its client retries them); tuned etcd fails a few hundred requests
  even on a slow disk, because 100 ms elections fire while the leader is merely slow.
- **A dead or cut-off leader** costs etcd with default timeouts one election: 1.6-1.8 s
  without completions. Tuned etcd recovers in about 150 ms. QuePaxa needs no election, but
  its followers wait one hedge delay before proposing themselves, so its gap (0.4-0.6 s) is
  between the two. Its hedge delay adapts to the commit latency but is capped by
  `hedge_max` (1 s); it is not tuned here.
- **A slow leader disk** barely touches QuePaxa: the two other recorders form the
  majority, so commits never wait for the slow one. Every etcd write goes through the
  leader, whose raft loop waits on its own log write before taking the next batch, so its
  p99 follows the slow disk for the whole fault.
- **A distant leader**: with default timeouts every etcd commit pays the added delay and
  nothing else changes. Tuned etcd does worse (p99 622 ms): 50 ms each way is close to
  its 100 ms election timeout. Which mechanism costs it the time was not traced.
  QuePaxa's followers hedge around the slow leader; its p99 lies between the two.
- **QuePaxa's weakness after a crash**: with one replica dead for the rest of the run, it
  stalled briefly (250-500 ms, several times) long after recovering, which is most of its
  3 s of degraded time; etcd did not. The cause is not yet diagnosed; the leader rotation
  is one candidate: with a member down, a rotation can hand leadership back to it, and
  slots then wait for a takeover again.
- Before the fix in #118, the same runs showed QuePaxa tails of 10-29 s after every fault:
  a follower's hedged writes lost slot after slot to a healthy new leader. The numbers
  above are after it.
- One host and one configuration: hypotheses to check on your own hardware, not
  production claims. All six nodes share the host's disk and CPU sockets, and the
  network is a Docker bridge.

## Not measured

- No comparison against the Go reference implementation: running the Go code was out of
  scope of the research (it was read, not built). The workload above mirrors its shape so
  a comparison can be added on the same machine.
- Heterogeneous WAN placements are covered by the simulator (`wan_leaders`) and one added
  delay in the etcd comparison, not by a multi-region deployment.

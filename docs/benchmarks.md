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
50% `PUT` / 50% `GET` (linearizable, then through log barriers), random keys from a 52-letter
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
`--etcd`; QuePaxa runs its default `Config`. Steady state and the read-heavy run measured
2026-10-31 with reads that take no log slot (`20ae43b`), raw results in
`target/bench-etcd/20261031-183234/`; faults 2026-10-30 with the hedge-delay fix
(`01bf73e`), `target/bench-etcd/20261030-233953/` (the fault load is write-only, and the
write path has not changed since).

### Steady state

etcd's own `benchmark` tool (closed loop, 256 clients over 16 connections, all three
members):

| Workload | etcd req/s | etcd p50 / p99 | QuePaxa req/s | QuePaxa p50 / p99 |
|---|---|---|---|---|
| put, unlimited | 24,221 | 10.4 / 19.5 ms | 31,985 | 7.3 / 13.7 ms |
| put, 5,000/s | 4,866 | 8.0 / 93.8 ms | 4,999 | 5.9 / 11.1 ms |
| txn-put | 22,580 | 10.9 / 22.7 ms | 32,650 | 7.2 / 12.7 ms |
| range, linearizable | 100,116 | 1.4 / 12.6 ms | 147,466 | 1.0 / 5.5 ms |
| range, serializable | 143,705 | 0.9 / 6.6 ms | 154,484 | 0.9 / 5.1 ms |

Writes: QuePaxa commits in fewer `fsync`s on the path (a decision is applied before its
local record is durable), so 30% more throughput at lower latency. Linearizable reads ask
a majority for its high-water mark and wait until the replica applied past it (ADR 0003):
one round trip, no log slot, no write, so they run close to serializable ones. etcd
confirms leadership with a heartbeat round (ReadIndex). Before this change every
linearizable read went through the log, a slot per batch: 33,648/s at p50 6.9 ms on the
same host (`target/bench-etcd/20261031-175233/`), a third of etcd's.

### Read-heavy

Open loop at 40,000 requests/s on all three members, 90% linearizable ranges and 10% puts
(`run.sh mixed`). A read now waits for the writes in flight past the replica's applied
prefix (they hold the mark up), where it used to share their slot, so it pays about one
round trip more:

| Version | p50 | p99 | max |
|---|---|---|---|
| reads through the log (`22a016b`) | 7.4 ms | 12.5-13.2 ms | 139-165 ms |
| reads without a log slot (`20ae43b`) | 8.0-8.2 ms | 14.4-15.3 ms | 34-141 ms |
| etcd | 9.7-10.7 ms | 21-26 ms | 37-115 ms |

Ranges over the runs without errors (four with reads through the log, three without a log
slot, alternated). The host is shared and was noisy
(load average 5-9 from other tenants): in noisy runs both QuePaxa versions and etcd
degraded alike (p99 into hundreds of ms, and QuePaxa refusing reads with `Busy` once 4,096
were outstanding), so those runs are left out rather than averaged. An earlier version of
the waiting rule drove every slot below the mark itself, contending with their proposers:
p99 45-200 ms here, fixed before these runs.

### Leader faults

Open-loop load (`kv-bench --protocol etcd --gets 0`, 5,000 puts/s, Poisson arrivals) on the two
followers for 40 s; the fault starts at 10 s and is undone at 25 s (a crashed leader stays
down). Latency runs from each request's due time; percentiles cover successful requests
only, so a request etcd fails after its internal 7 s timeout is an error, not a 7 s sample.
etcd runs with its default timeouts (heartbeat 100 ms, election 1 s) and tuned for a fast
local network (10 ms, 100 ms).

| Fault | System | p99 | max | Errors | Longest gap | Degraded |
|---|---|---|---|---|---|---|
| crash (`SIGKILL`) | etcd | 16 ms | 1,087 ms | 5,402 | 1,075 ms | 1,250 ms |
| | etcd, tuned | 16 ms | 139 ms | 603 | 127 ms | 250 ms |
| | QuePaxa | 15 ms | 130 ms | 0 | 40 ms | 0 ms |
| partition (15 s) | etcd | 75 ms | 1,094 ms | 5,483 | 1,082 ms | 2,000 ms |
| | etcd, tuned | 16 ms | 141 ms | 613 | 124 ms | 250 ms |
| | QuePaxa | 12 ms | 138 ms | 0 | 36 ms | 250 ms |
| slow disk (50 write IOPS, 15 s) | etcd | 395 ms | 506 ms | 0 | 227 ms | 4,750 ms |
| | etcd, tuned | 391 ms | 700 ms | 628 | 385 ms | 1,750 ms |
| | QuePaxa | 14 ms | 176 ms | 0 | 160 ms | 250 ms |
| far leader (+50 ms, 15 s) | etcd | 243 ms | 535 ms | 0 | 90 ms | 500 ms |
| | etcd, tuned | 1,126 ms | 1,716 ms | 0 | 548 ms | 1,750 ms |
| | QuePaxa | 13 ms | 166 ms | 0 | 40 ms | 250 ms |

What the table says, and what it does not:

- **No request failed on QuePaxa** in any scenario. etcd answers clients with errors while
  it has no leader (its client retries them): thousands with default timeouts, hundreds
  tuned, and tuned etcd fails requests even when the leader is only slow, because its
  100 ms elections fire anyway.
- **A dead or cut-off leader** costs etcd with default timeouts one election: about 1.1 s
  without completions. Tuned etcd recovers in about 130 ms. QuePaxa needs no election:
  followers propose themselves after the hedge delay (four times the smoothed commit
  latency, a few tens of ms here) and the leader rotates out after eight slots it did not
  lead, so its longest gap is about 40 ms. etcd's p99 is as low as QuePaxa's because the
  requests it fails during the election leave the percentiles; QuePaxa delays them by at
  most 140 ms instead.
- **A slow leader disk** barely shows in QuePaxa's numbers: the two other recorders form
  the majority, so commits never wait for the slow one. Every etcd write goes through the
  leader, whose raft loop waits on its own log write before taking the next batch, so its
  p99 follows the slow disk for the whole fault.
- **A distant leader**: QuePaxa's followers hedge around it within tens of ms and rotate it
  out, so its p99 stays at the level of a healthy cluster. etcd with default timeouts pays
  the added delay on every commit for the whole fault; tuned etcd does worse (p99 1.1 s),
  with 50 ms each way close to its 100 ms election timeout. Which mechanism costs it the
  time was not traced.
- QuePaxa's slow-disk row varies between runs: in six reruns its longest gap ranged from
  26 to 296 ms and its max from 160 to 313 ms, with brief stalls at random times during and
  after the fault. A trace of one showed the mechanism: while the throttled replica's
  writeback slows the shared disk, the healthy leader's commits briefly exceed the hedge
  delay (about 20 ms), the follower hedges every waiting entry at once (600 in 250 ms),
  and those proposals contend with the leader's for the same slots, stretching a hiccup of
  tens of ms to 150-300 ms. The code before the hedge-delay fix behaves alike (max 150-200 ms
  in five reruns). Limiting how much a follower hedges at once would damp it, at the cost
  of a slower rotation away from a dead leader; it is not done.
- etcd's numbers vary between runs more than QuePaxa's: in an earlier run on the same
  host, default etcd's p99 after a crash was 110 ms instead of 16 ms and after a slow
  disk 1.5 s instead of 395 ms, depending on where the election and the backlog fell.
- These are after two QuePaxa fixes this comparison found. Before #118, a follower's
  hedged writes lost slot after slot to a healthy new leader (tails of 10-29 s). Before
  the next fix, the hedge delay learned from hedged entries, whose latency contains the
  delay itself, so once the leader slowed it rose to its 1 s cap within a second: a 0.3 s
  gap after a crash and p99 680 ms with a distant leader.
- In an earlier run with half the load as linearizable ranges, QuePaxa stalled briefly
  (250-500 ms, a few times) long after a crash. Three reruns with leader changes logged
  did not reproduce it (one rotation each, as expected), and etcd showed similar dips in
  write-only runs on the same shared disk, so `fsync` jitter on the two remaining replicas
  is the likelier cause.
- One host and one configuration: hypotheses to check on your own hardware, not
  production claims. All six nodes share the host's disk and CPU sockets, and the
  network is a Docker bridge.

### Across regions

Five replicas of each system, one per emulated region (us-east, us-west, eu-west, ap-ne,
sa-east; round trips of 70-300 ms, the `wan_leaders` matrix), on the same host with
`tc netem` delaying every replica-to-replica link (`examples/kv-tokio/bench/etcd/wan.sh`).
A generator per region writes 200 puts/s to its own region's replica (1,000/s in total)
for 40 s; faults hit the leader's region from 10 s to 25 s. The leader is pinned to ap-ne,
whose second-nearest peer is the farthest (180 ms; a majority is three of five), or to
us-east (80 ms) for the best-placed control. QuePaxa runs with WAN timers (retransmit
400 ms, hedge cap 3 s), etcd with its defaults (heartbeat 100 ms, election 1 s) and tuned
per its guide for the 300 ms largest round trip (300 ms, 3 s). Medians of six runs (two
sets of three, `target/bench-etcd/wan-20261031-235204/` and `wan-20261101-011133/`, at
`876bc6b`), range in parentheses, latency in ms.

With the leader's region alive, on all five regions' writes:

| Leader's region | QuePaxa p99 | etcd p99 | etcd tuned p99 | Errors (QuePaxa / etcd / tuned) |
|---|---|---|---|---|
| far, no fault | 654 (612-683) | 2,754 (1,006-4,038) | 1,122 (892-1,460) | 0 / 0 (0-381) / 0 |
| best-placed, no fault | 327 (320-333) | 504 (491-844) | 498 (497-967) | 0 / 0 / 0 |
| far, +100 ms on its links | 1,281 (1,081-2,324) | 2,327 (2,104-4,032) | 1,391 (1,186-1,927) | 0 / 0 / 0 |
| far, ±50 ms jitter and 1% loss | 1,798 (1,193-11,795) | 6,672 (6,491-6,843) | 6,305 (4,999-9,804) | 0 / 4,511 (3,998-11,056) / 0 (0-1,727) |

With the leader's region crashed or cut off, on the other four regions' writes (its own
clients cannot write at all):

| Fault | System | Errors | Longest gap | p99 | max |
|---|---|---|---|---|---|
| crash | QuePaxa | 0 | 3,231 (3,204-3,453) | 6,998 (6,564-7,553) | 7,915 |
| | etcd | 1,172 (947-1,360) | 1,807 (1,709-2,284) | 2,722 (2,299-5,692) | 3,842 |
| | etcd, tuned | 3,063 (2,667-4,327) | 4,792 (3,454-6,021) | 1,977 (1,416-6,081) | 5,473 |
| partition | QuePaxa | 0 | 3,228 (3,179-3,300) | 7,090 (6,913-8,135) | 8,001 |
| | etcd | 1,223 (1,162-1,476) | 2,293 (1,752-2,532) | 3,121 (2,483-4,988) | 4,140 |
| | etcd, tuned | 2,904 (2,843-3,861) | 4,308 (4,123-6,056) | 1,782 (1,046-4,018) | 5,029 |

What the tables say, and what they do not:

- **No QuePaxa request failed** in any case. etcd fails writes whenever it loses or
  changes its leader: with jitter on the leader's links default etcd's 1 s election
  timeout fires again and again (4,000-11,000 errors per run), and once it fired with no
  fault at all.
- **With the leader alive but far or jittery**, QuePaxa's p99 is the lowest of the three.
  With +100 ms on the leader's links it is about even with tuned etcd (the two sets
  disagreed on the order). Placement matters for all three: moving the leader from ap-ne
  to us-east at least halves the p99.
- **Losing the leader's region costs QuePaxa a 3.2 s stall** in every other region: longer
  than default etcd's election (about 2 s), shorter than tuned etcd's (4-5 s). The gap is
  close to the hedge cap (3 s) plus a round trip: followers propose their own entries only
  after the hedge delay, which on these round trips reaches the cap. After the stall the
  backlog drains slowly, so QuePaxa's p99 (7-8 s) is well above the gap; etcd's p99 is
  lower partly because the requests it fails leave the percentiles.
- **One jitter run stalled QuePaxa for 11.6 s** (p99 11.8 s); the other five stalled
  0.7-1.7 s. Not explained yet. This, the 3 s stall and the slow drain are follow-up work
  for the engine (`STRATEGY.md`, cross-region track), not fixed here.
- The two sets reproduce each other: per system and case their p99 ranges overlap, except
  default etcd with jitter (6,491-6,672 against 6,722-6,843) and tuned etcd after a crash
  (1,416-1,977 against 2,129-6,081), and QuePaxa and tuned etcd rank the same in each set
  on p99 and longest gap, except the p99 with +100 ms noted above.
- The `Degraded` column of `summary.md` is left out: at 200 writes/s per region, 250 ms
  buckets hold about 50 requests, and round-trip variation alone puts many of them under
  the 80% line even without a fault.
- Jittered links are rate-limited in netem so that it does not reorder packets; the
  replicas saw 3-36 TCP reordering events per jitter run, from the loss.
- In these runs the leader after the fault was found with short-lived probe containers
  during the last 7.5 s of the load, not pinned off the replicas' cores; `wan.sh` now
  probes after the load. The post-heal tail may carry a little of that noise.
- One host: links are emulated, there is no bandwidth limit, and the five replicas share
  the disk and CPUs. Hypotheses for real regions, not production claims.

## Not measured

- No comparison against the Go reference implementation: running the Go code was out of
  scope of the research (it was read, not built). The workload above mirrors its shape so
  a comparison can be added on the same machine.
- No deployment on real regions: the cross-region comparison above emulates the links on
  one host.

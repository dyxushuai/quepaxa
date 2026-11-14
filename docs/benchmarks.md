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
per replica, `kv-tokio`'s setting then; 65,536 since `812f26f`) pushes back with `Busy`. Before #50 it was near 100k/s (p90 30.7 ms, p99
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

### At saturation (#155)

Three replicas with data on tmpfs, each pinned to two CPUs (`taskset` 0-1, 2-3, 4-5),
`kv-bench` on 8-15 with the `run-local.sh` workload (24 connections, half linearizable
gets), 12-20 s per point, at `ee49e4a` with the `profiling` build and frame pointers.
Measured 2026-11-13.

| Offered rate | Achieved | Errors | p50 | p90 | p99 |
|---|---|---|---|---|---|
| 150,000/s | 150,045/s | 0 | 0.48 ms | 0.99 ms | 65 ms |
| 200,000/s | 199,949/s | 0 | 0.58 ms | 17 ms | 105 ms |
| 250,000/s | 250,090/s | 0 | 0.88 ms | 119 ms | 295 ms |
| 400,000/s | 400,133/s | 0 | 1.3 ms | 145 ms | 247 ms |
| 500,000/s | 500,258/s | 0 | 4.0 ms | 230 ms | 1,067 ms |

Throughput keeps up past 500k/s (with `max_outstanding_proposals` at 65,536 nothing is
refused), but the tail grows from 200k/s on. At 250k/s, 8 s of `perf record -F 2999` on
all three processes: each replica's event-loop thread is busy for 93-99% of the time (one
full core), its two Tokio workers 30-34% each, so the event loop saturates first, not
the outbound I/O. Hedges (10,703-127,537 per replica per run) and leader rotations
(`suspicions` 3-8) show the queueing it causes. Where the event-loop thread's time goes
(55,489 samples):

| Share | Where |
|---|---|
| 49% | kv-tokio's apply (`ops::run`): `BTreeMap` searches in `Kv::put` and `Kv::range`, mostly `memcmp` (19% of the thread on its own) |
| 15% | the KV's shard copy-on-write after each snapshot (`BTreeMap::clone`) |
| 12% | `Replica::handle` (decoding and handling peer messages) |
| 11% | the rest of `Node` (draining actions, replies, sends) |
| 3.5% | `Replica::propose` |
| 2.8% | timers (`handle_timeout`, `next_deadline`) |
| 3% | other core work (`pump`, `Record::encode` 1.7%) |

The core takes about a fifth of the thread; decoding is a small part of that. Moving
decoding to the host (an API change that would hand the replica pre-decoded messages)
would save a few percent of one thread and is not worth it. The host's state machine is
the bottleneck: `Kv::put` searches each key twice (`get`, then `insert`), and every
snapshot makes the next write to each shard copy it. Data: `target/eval-results/saturation-155/`.

### One lookup per write, no shard copy (#210)

`Kv::put` now searches its shard once (`BTreeMap::entry`), and a single-key delete removes
directly instead of a range lookup followed by a remove. A write to a shard that a snapshot
still holds goes to a small per-shard overlay rather than copying the shard; the first
write after the snapshot is dropped folds the overlay back in. The event loop's cost for a
snapshot is therefore proportional to the writes made during encoding, not to the state.

Same layout and workload as above, with peers on 27100-27102 and clients on 27200-27202,
before (`ebc4f34`) and after (`cb1aab0`) runs interleaved, three 15 s runs per point, the whole
matrix under `flock target/ops/bench.lock` with no other process above 50% CPU. Measured
2026-11-14.

| Offered rate | Build | Achieved | p50 | p90 | p99 | Hedges per replica |
|---|---|---|---|---|---|---|
| 200,000/s | before | 199,995/s | 0.60-0.62 ms | 39-41 ms | 128-139 ms | 5,348-43,804 |
| 200,000/s | after | 199,996/s | 0.58-0.59 ms | 2.5-3.1 ms | 16-20 ms | 0-912 |
| 250,000/s | before | 250,039/s | 0.75-0.78 ms | 70-84 ms | 154-198 ms | 5,326-43,942 |
| 250,000/s | after | 250,039/s | 0.69-0.73 ms | 10-12 ms | 39-42 ms | 0-3,283 |

p90 at 250k/s (10-12 ms) is now below its value at 200k/s before the change (39-41 ms).
Leader suspicions per replica dropped from 2-14 per run to 0-3. Event-loop thread, 8 s of
`perf record -F 2999` at 250k/s, shares of that thread's samples (ranges over the three
replicas):

| | Before | After |
|---|---|---|
| Thread busy | 70-73% | 63-64% |
| kv-tokio apply (`ops::run`) | 47-51% | 42-43% |
| of it `Kv::put` | 29-32% | 20-21% |
| `BTreeMap::clone` (shard copy-on-write) | 11-14% | 0.01% |
| overlay fold (`Shard::owned`) | – | 4.2-4.3% |
| `Replica::handle` | 10-17% | 14-15% |
| libc `memcmp` (self) | 20% | 24% |

The next bottleneck is the key comparisons in `BTreeMap` searches: `memcmp`'s sample count
is unchanged by this change, and it is the largest single symbol on the thread. Each compare
follows an `Arc<[u8]>` to its heap bytes. After that comes the core's `Replica::handle`.
Data: `target/eval-results/kv-apply-210/locked2/`.

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

## Order change in time (`cargo test --release -p quepaxa-sim --test order_change -- --ignored --nocapture`)

A membership or leader-order change decided in slot `s` governs slots from `s + 64` on
(`LAG`). How long that takes in time (#150): the leader (replica 0) proposes the same members
with another leader first while clients write at a fixed rate spread over all five
replicas. Measured from the first replica to report the change decided to the agreed log
reaching its first slot, 3 seeds (min-max), fault-free. WAN: the placement above with
us-east leading, WAN timers. LAN: 1-2 ms one-way, the library's default timers (50 ms
retransmit, 1 s hedge cap). Both run 8 slots in flight. The decision itself takes one
commit (80-130 ms on the WAN, under 10 ms on the LAN).

| Writes/s | WAN | LAN |
|---|---|---|
| idle | 3.5-3.6 s | 189-230 ms |
| 1 | 3.5-4.2 s | 132-208 ms |
| 10 | 3.2-3.4 s | 142-214 ms |
| 100 | 0.76-0.85 s | 173-185 ms |
| 1,000 | 0.67-0.71 s | 64-69 ms |

With few writes the leader fills the 64 slots with empty batches, a window of in-flight
slots at a time after a hedge delay; from about 100 writes/s writes fill them sooner. Even
idle, a change takes effect within about 4 s: over 40 times shorter than a sustained period
of about 3 minutes and over 140 times shorter than a hold of about 10 minutes after an
order change, so `LAG` does not constrain those defaults.

## Against etcd

The [follow-up at `617ee8f`](optimize/etcd-degraded-leader-report.md) excludes
local measurement artifacts from the Docker build and verifies one identical
binary across three far-leader repetitions. QuePaxa's median p99 is 13.1 ms
versus default etcd's 165.8 ms, and its median longest gap is 26 ms versus
55 ms, both without errors. The report preserves the earlier rebuilt samples,
WAN results, diagnostic traces, and their uncertainty. No new runtime gain
is claimed; the tables below retain their original revisions.

`examples/kv-tokio/bench/etcd/run.sh` (method, fairness rules and how to read each column:
[its README](../examples/kv-tokio/bench/etcd/README.md)). Three `kv-tokio` replicas and
three etcd v3.7.2 members, one system at a time, each node in its own container with
three cores, data on the VM's ext4 disk (one 512-byte append + `fsync`: 1.7 ms mean). Both
are driven through etcd's v3 KV gRPC API by the same clients, which `kv-tokio` serves with
`--etcd`; QuePaxa runs its default `Config` except `max_outstanding_proposals`, which
`kv-tokio` raises to 65,536. Measured 2026-11-03 at `77c0577`, with followers that stop
waiting for a silent leader (`silent_leader_takeover`), raw results in
`target/bench-etcd/20261103-030942/`.

### Steady state

etcd's own `benchmark` tool (closed loop, 256 clients over 16 connections, all three
members):

| Workload | etcd req/s | etcd p50 / p99 | QuePaxa req/s | QuePaxa p50 / p99 |
|---|---|---|---|---|
| put, unlimited | 23,761 | 10.6 / 19.5 ms | 30,113 | 7.4 / 16.4 ms |
| put, 5,000/s | 4,998 | 7.9 / 15.6 ms | 4,997 | 5.9 / 11.1 ms |
| txn-put | 23,055 | 10.8 / 21.2 ms | 31,499 | 7.4 / 13.5 ms |
| range, linearizable | 98,665 | 1.4 / 12.5 ms | 152,542 | 1.0 / 5.2 ms |
| range, serializable | 144,853 | 1.0 / 6.2 ms | 151,537 | 0.9 / 5.4 ms |

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
| the same, 65,536 outstanding (`812f26f`, `77c0577`) | 8.1 ms | 14.5-14.6 ms | 30-35 ms |
| etcd | 9.7-10.7 ms | 21-26 ms | 37-115 ms |

Ranges over the runs without errors (four with reads through the log, three without a log
slot, alternated; one each at `812f26f` and `77c0577`, etcd 21.6-22.1 ms p99 in both). The host is shared and was noisy (load average 5-9
from other tenants): in noisy runs both QuePaxa versions and etcd degraded alike (p99 into
hundreds of ms), and QuePaxa refused reads with `Busy` once 4,096 were outstanding, so
those runs are left out rather than averaged. To reproduce the noise on purpose, busy
loops pinned to the nodes' and generator's cores (0-12) ran alongside the benchmark: with 4
and 7 of them QuePaxa's p99 was 14.6 and 19.3 ms against etcd's 68 and 86 ms; with 10 the
host is overloaded, and with the 4,096 limit QuePaxa failed 24,339 requests with `Busy`
(p99 3.7 s) where etcd failed none (p99 9.1 s). With 65,536 the same overload costs
QuePaxa no request (p99 0.4-0.7 s over two runs) and etcd 22,507-195,993 (p99 12-19 s). An earlier version of
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
| crash (`SIGKILL`) | etcd | 18 ms | 1,069 ms | 5,184 | 1,060 ms | 1,250 ms |
| | etcd, tuned | 16 ms | 136 ms | 536 | 123 ms | 250 ms |
| | QuePaxa | 12 ms | 39 ms | 0 | 13 ms | 0 ms |
| partition (15 s) | etcd | 518 ms | 1,943 ms | 5,395 | 1,931 ms | 2,000 ms |
| | etcd, tuned | 819 ms | 1,192 ms | 641 | 772 ms | 2,500 ms |
| | QuePaxa | 42 ms | 118 ms | 0 | 101 ms | 0 ms |
| slow disk (50 write IOPS, 15 s) | etcd | 982 ms | 1,395 ms | 0 | 544 ms | 5,250 ms |
| | etcd, tuned | 236 ms | 415 ms | 535 | 198 ms | 2,250 ms |
| | QuePaxa | 27 ms | 160 ms | 0 | 51 ms | 0 ms |
| far leader (+50 ms, 15 s) | etcd | 170 ms | 295 ms | 0 | 86 ms | 250 ms |
| | etcd, tuned | 1,867 ms | 2,222 ms | 85 | 839 ms | 3,500 ms |
| | QuePaxa | 18 ms | 249 ms | 0 | 225 ms | 0 ms |

What the table says, and what it does not:

- **No request failed on QuePaxa** in any scenario. etcd answers clients with errors while
  it has no leader (its client retries them): thousands with default timeouts, hundreds
  tuned, and tuned etcd fails requests even when the leader is only slow, because its
  100 ms elections fire anyway.
- **A dead or cut-off leader** costs etcd with default timeouts one election: 1.1-1.9 s
  without completions. Tuned etcd recovers in 0.1-0.8 s. QuePaxa needs no election: a
  follower that hears nothing from the leader for about a commit latency after forwarding
  hands its writes to the next member of the leader order and takes over the leader's
  unfinished slots, and the leader rotates out after eight slots it did not lead. Its
  longest gap was 13 ms after a crash and 101 ms after a partition. The partition row
  varies with host load: in three quiet reruns QuePaxa's longest gap was 13-15 ms and its
  p99 12-13 ms; in six noisy ones, alternated with the code before the silence wait lost
  its `retransmit_interval` floor, 36-345 ms (median 144 ms) against 158-777 ms (median
  538 ms). In one earlier full run tuned etcd's p99 after a partition (19 ms) was below
  QuePaxa's (68 ms), with 640 requests failed and so left out of its percentiles.
- **QuePaxa's longest gap with a distant leader** (225 ms here) is the one cell above
  default etcd's (86 ms), and not only in this run: over ten runs of `812f26f` and
  `77c0577` it ranged from 20 to 743 ms (median about 140 ms), against 86-96 ms for
  default etcd in three. The stalls fall mostly while the leader is distant and grow with
  host load; QuePaxa's p99 stayed below default etcd's in eight of the ten. Those
  earlier runs were not traced; see the follow-up above for one later diagnostic.
- **A slow leader disk** barely shows in QuePaxa's numbers: the two other recorders form
  the majority, so commits never wait for the slow one. Every etcd write goes through the
  leader, whose raft loop waits on its own log write before taking the next batch, so its
  p99 follows the slow disk for the whole fault.
- **A distant leader**: QuePaxa's followers hedge around it within tens of ms and rotate it
  out, so its p99 stays at the level of a healthy cluster. etcd with default timeouts pays
  the added delay on every commit for the whole fault; tuned etcd does worse (p99 1.1 s),
  with 50 ms each way close to its 100 ms election timeout. Which mechanism costs it the
  time was not traced.
- QuePaxa's slow-disk row varies between runs: in six earlier reruns its longest gap
  ranged from 26 to 296 ms and its max from 160 to 313 ms (76-210 ms in three noisy ones at
  `6a2720d`), with brief stalls at random times during and
  after the fault. A trace of one showed the mechanism: while the throttled replica's
  writeback slows the shared disk, the healthy leader's commits briefly exceed the hedge
  delay (about 20 ms), the follower hedges every waiting entry at once (600 in 250 ms),
  and those proposals contend with the leader's for the same slots, stretching a hiccup of
  tens of ms to 150-300 ms. The code before the hedge-delay fix behaves alike (max 150-200 ms
  in five reruns). The simulator reproduces it (`quepaxa-sim/tests/disk_faults.rs`): a
  shared disk that suddenly slows from 2 to 40 ms per write for half a second set off about
  400 hedges and writes up to 210-410 ms, where a disk that slow all along gives at most
  about 110 ms. Since `428a2e7`, a follower waits four hedge delays (at most `hedge_max`)
  before hedging an entry if its target has decided a slot itself since the forward, and
  hedges around a talking target one slot at a time: no hedges and at most 79-113 ms in
  the simulator. In three alternated `run.sh slow-disk` runs against the code before
  (noisy host), the median longest gap fell from 357 to 56 ms and the median p99 from 211
  to 38 ms; crash, partition and far leader stayed within each other's ranges, as did the
  five-region crash and jitter cases.
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
- **A stuck leader disk** (`run.sh stuck-disk`: the leader's writes capped at 1 IOPS for
  15 s, clients on every member, one run, `target/bench-etcd/20261106-161556/`): QuePaxa
  fails nothing, p99 23 ms, max 123 ms, longest gap 104 ms. etcd's leader stalls on its
  log: default etcd fails 55,559 requests and leaves 3,630 unanswered (longest gap
  1,180 ms), tuned etcd 46,378 and 230 (761 ms). Before the leader stepped aside (`main`
  at `bfdec81`, a QuePaxa-only probe with the same fault, three runs each, alternated),
  writes from its own clients waited for its disk: p99 43-47 ms and max 567-579 ms,
  against 12-18 ms and 126-207 ms after.
- **Four times the load** (`RATE=20000 run.sh crash partition slow-disk`, one run at
  `06bc3d7`, `target/bench-etcd/20261105-170501/`): QuePaxa fails nothing and its longest
  gaps are 112, 14 and 49 ms; default etcd fails 8,473-54,516 requests with gaps of
  360-1,542 ms, tuned etcd 2,735-4,766 requests with 127-148 ms. QuePaxa's p99 is 17-44 ms
  against tuned etcd's 76-220 ms.
- **Rejoining.** A replica cut off by the partition used to rejoin about 11 s after the
  heal (three runs): `kv-tokio`'s writes on the dead connection waited for TCP's own
  retransmission backoff. Since `06bc3d7` a peer connection that takes no bytes for 2 s
  is redialed, and it rejoins in about 0.6 s. A leader killed and restarted after 15 s
  decides slots again 0.4-0.5 s after it starts, with no client-visible gap (11 ms longest,
  two runs). These come from a QuePaxa-only probe with the same load and timing that
  samples `STATS` every 100 ms; `run.sh` has no restart scenario.
- One host and one configuration: hypotheses to check on your own hardware, not
  production claims. All six nodes share the host's disk and CPU sockets, and the
  network is a Docker bridge.

### Five members

`NODES=5 run.sh` (two cores per node, generator on 10-13), one full run at `573d78c`
(`target/bench-etcd/20261106-171807/`), the read-heavy and slow-disk cases repeated.

| Workload | etcd req/s | etcd p50 / p99 | QuePaxa req/s | QuePaxa p50 / p99 |
|---|---|---|---|---|
| put, unlimited | 19,036 | 13.2 / 24.5 ms | 30,751 | 7.6 / 14.4 ms |
| put, 5,000/s | 4,997 | 11.1 / 21.0 ms | 4,998 | 6.2 / 11.7 ms |
| txn-put | 18,178 | 13.7 / 26.7 ms | 30,876 | 7.5 / 13.7 ms |
| range, linearizable | 89,203 | 1.7 / 17.7 ms | 142,017 | 1.1 / 5.4 ms |
| range, serializable | 139,880 | 1.0 / 7.7 ms | 150,367 | 1.0 / 5.2 ms |

| Fault | System | p99 | max | Errors | Longest gap |
|---|---|---|---|---|---|
| crash | etcd | 21 ms | 1,076 ms | 5,237 | 1,063 ms |
| | etcd, tuned | 21 ms | 146 ms | 629 | 127 ms |
| | QuePaxa | 14 ms | 178 ms | 0 | 13 ms |
| partition | etcd | 21 ms | 1,229 ms | 6,163 | 1,210 ms |
| | etcd, tuned | 22 ms | 174 ms | 723 | 157 ms |
| | QuePaxa | 51 ms | 274 ms | 0 | 49 ms |
| slow disk | etcd | 785-821 ms | 993-1,066 ms | 0 | 262-560 ms |
| | etcd, tuned | 21-339 ms | 131-433 ms | 589-592 | 117-127 ms |
| | QuePaxa | 18-116 ms | 131-437 ms | 0 | 102-156 ms |
| stuck disk | etcd | 24 ms | 1,806 ms | 32,637 (1,815 unanswered) | 1,787 ms |
| | etcd, tuned | 21 ms | 147 ms | 28,072 (136 unanswered) | 131 ms |
| | QuePaxa | 15 ms | 151 ms | 0 | 103 ms |
| far leader | etcd | 198 ms | 397 ms | 0 | 50 ms |
| | etcd, tuned | 1,453 ms | 1,909 ms | 2,016 | 777 ms |
| | QuePaxa | 34 ms | 383 ms | 0 | 25 ms |

- Five members cost both systems some throughput (two cores each instead of three, and a
  majority of three); QuePaxa keeps about 60% more writes and linearizable reads than etcd.
- **Read-heavy** (40,000 requests/s, 90% linearizable ranges): QuePaxa's p99 was 15-16 ms
  in three of four runs against etcd's 28-30 ms, and 128 ms in the fourth (max 528 ms);
  a first run, while this host was also compiling, gave 2,478 ms. Natively, without
  Docker's bridge, the same five-node load runs at p99 2.5 ms. QuePaxa's containers used
  35-50% of their two cores, so it is not CPU-bound; the occasional tail is not traced.
- The slow-disk row is two runs; the one longest gap QuePaxa lost (156 against tuned
  etcd's 127 ms) became 102 against 117 ms in the second.

### Across regions

Five replicas of each system, one per emulated region (us-east, us-west, eu-west, ap-ne,
sa-east; round trips of 70-300 ms, the `wan_leaders` matrix), on the same host with
`tc netem` delaying every replica-to-replica link (`examples/kv-tokio/bench/etcd/wan.sh`).
A generator per region writes 200 puts/s to its own region's replica (1,000/s in total)
for 40 s; faults hit the leader's region from 10 s to 25 s. The leader is pinned to ap-ne,
whose second-nearest peer is the farthest (180 ms; a majority is three of five), or to
us-east (80 ms) for the best-placed control. QuePaxa runs with WAN timers (retransmit
400 ms, hedge cap 3 s), etcd with its defaults (heartbeat 100 ms, election 1 s) and tuned
per its guide for the 300 ms largest round trip (300 ms, 3 s). Medians of three runs, range
in parentheses, latency in ms: QuePaxa at `77c0577` (`target/bench-etcd/wan-20261103-032219/`),
etcd from the night before with the same harness (`wan-20261102-225414/`). QuePaxa's rows
from before silent-leader takeover are from `876bc6b` and `bb2f5dc`
(`wan-20261031-235204/`, `wan-20261101-011133/`, `wan-20261101-030954/`).

With the leader's region alive, on all five regions' writes:

| Leader's region | p99 (QuePaxa / etcd / tuned) | Longest gap (QuePaxa / etcd / tuned) | Errors (QuePaxa / etcd / tuned) |
|---|---|---|---|
| far, no fault | 655 (633-655) / 3,781 (3,153-4,067) / 1,229 (1,070-1,354) | 191 / 318 / 363 | 0 / 0 (0-595) / 0 |
| best-placed, no fault | 339 (293-362) / 515 (488-515) / 510 (502-514) | 183 / 201 / 370 | 0 / 0 / 0 |
| far, +100 ms on its links | 923 (830-926) / 3,166 (2,038-3,825) / 1,207 (1,117-1,657) | 289 / 412 / 428 | 0 / 0 / 0 |
| far, jitter (σ 50 ms) and 1% loss on its links | 1,435 (862-3,111) / 6,423 (6,290-6,522) / 6,880 (6,262-8,182) | 614 / 11,139 / 5,511 | 0 / 2,527 (998-4,497) / 3 (0-1,262) |

With the leader's region crashed or cut off, on the other four regions' writes (its own
clients cannot write at all):

| Fault | System | Errors | Longest gap | p99 | max |
|---|---|---|---|---|---|
| crash | QuePaxa | 0 | 859 (705-860) | 974 (898-977) | 1,817 |
| | QuePaxa before | 0 | 3,248 (3,204-3,453) | 7,089 (6,564-7,553) | 8,049 |
| | etcd | 1,273 (1,155-1,631) | 2,224 (2,048-2,528) | 4,233 (4,097-5,533) | 5,789 |
| | etcd, tuned | 3,747 (3,533-4,036) | 4,967 (4,687-5,658) | 3,145 (2,467-5,507) | 5,458 |
| partition | QuePaxa | 0 | 708 (685-794) | 904 (870-919) | 1,624 |
| | QuePaxa before | 0 | 3,249 (3,179-3,300) | 7,287 (6,913-8,135) | 8,140 |
| | etcd | 1,366 (1,225-1,388) | 2,300 (2,266-2,675) | 2,732 (2,455-2,934) | 3,413 |
| | etcd, tuned | 3,419 (3,062-4,074) | 4,628 (4,132-6,497) | 3,110 (1,060-3,747) | 5,234 |

What the tables say, and what they do not:

- **No QuePaxa request failed** in any case. etcd fails writes whenever it loses or
  changes its leader: with jitter on the leader's links both etcd configurations elect
  again and again (default etcd 1,000-4,500 errors per run), and default etcd's 1 s
  election timeout fired once with no fault at all.
- **With the leader alive but far, slow or jittery**, QuePaxa has the lowest p99 and the
  shortest longest gap of the three in every case. Placement matters for all three:
  moving the leader from ap-ne to us-east at least halves the p99.
- **Losing the leader's region costs QuePaxa 0.7-0.9 s** in every other region, down from
  3.2 s, against about 2.2 s for default etcd's election and 4.6-5 s for tuned etcd's;
  the p99 after the loss fell from 7-8 s to about 1 s. Before, followers proposed their
  own entries only after the hedge delay, which on these round trips reaches its 3 s cap,
  and the leader's unfinished slots waited longer still. Now a follower that hears
  nothing from the leader for about a commit latency after forwarding hands its entries
  to the next member of the leader order and takes over the leader's slots
  (`silent_leader_takeover`).
- **The leader's acknowledgements must stay rare.** A leader acknowledges a forward at
  once so that waiting on its disk does not look like silence. One acknowledgement per
  forward tripled QuePaxa's p99 in the jitter case (3.2-3.5 s against 0.96-1.0 s without,
  and 7.4 s with takeover off): every message is a packet the 1% loss can drop, stalling
  the TCP stream behind it. At most one per eighth of `retransmit_interval` per follower
  brought it back (0.93 s in a separate set of three, 1.4 s here; the code before
  takeover measured 0.89 s at `bb2f5dc` and 1.0 s at `5d6d384` on the day of these runs).
- **The jitter case is mostly extra, uneven delay**, not ±50 ms: netem keeps jittered
  packets in order only when the link has a rate, and with one it holds each packet
  behind the one ahead, so under load the added delay skews toward the high end. The
  replicas saw 3-61 TCP reordering events per jitter run, from the loss.
- **Before the harness fix**, injecting and healing the slow and jitter faults replaced
  the link's qdisc and dropped every queued packet, a loss burst at 10 s and 25 s. Those
  runs had QuePaxa and tuned etcd about even with +100 ms (p99 1,281 against 1,391) and
  one QuePaxa jitter run stalled for 11.6 s; neither recurred after the fix.
- The `Degraded` column of `summary.md` is left out: at 200 writes/s per region, 250 ms
  buckets hold about 50 requests, and round-trip variation alone puts many of them under
  the 80% line even without a fault.
- One host: links are emulated, there is no bandwidth limit, and the five replicas share
  the disk and CPUs. Hypotheses for real regions, not production claims.

### A slow member (`wan.sh heterogeneous`, #195)

The `heterogeneous` scenario has no WAN delay and puts one container on a CPU quota
(`SLOW_CPUS`, 0.5). Its settled commit p50 was about 18 ms in some runs and 5.7 ms in
others, whichever member led, which looked like the slow member on the commit path. It is
not: the shared virtual disk switches between two `fsync` regimes, and commit latency
follows it in every region at once, with or without a quota.

Six back-to-back 300 s runs at `ee49e4a`, alternating no effective quota (`SLOW_CPUS=2` on
the two-CPU cpuset) and a 0.5 CPU quota on a follower (`SLOW_REGION=us-west`), with the
leader pinned to ap-ne (`POLICY="--placement off"`). Alongside, a container appended 512
bytes with `fsync` to the same data disk once a second. p50 in ms over the settled window
(240-300 s), per origin region:

| Run | Quota | Probe `fsync` p50 | us-east | us-west (slow) | eu-west | ap-ne (leader) | sa-east |
|---|---|---|---|---|---|---|---|
| 1 | none | 4.6 | 5.7 | 5.8 | 5.8 | 5.5 | 5.6 |
| 2 | us-west 0.5 | 8.6 | 18.6 | 18.8 | 18.9 | 18.1 | 18.5 |
| 3 | none | 8.0 | 17.5 | 17.5 | 17.6 | 16.9 | 17.1 |
| 4 | us-west 0.5 | 8.4 | 17.7 | 17.9 | 17.9 | 17.4 | 17.7 |
| 5 | none | 8.4 | 18.7 | 18.8 | 18.8 | 18.3 | 18.5 |
| 6 | us-west 0.5 | 8.2 | 17.8 | 17.9 | 18.0 | 17.3 | 17.6 |

- **The step is the disk's.** In every run that switched (2, 3, 5), all five regions went
  from 5.7 to about 18 ms within the same 30 s window as the probe's `fsync` p50 went from
  about 4.5 to about 8.5 ms; run 1 started in the slow regime (13.5 ms in its first minute)
  and left it. Single probe `fsync`s took up to 3.3 s around the switch. The guest's IO
  pressure barely moves (`some` avg10 from about 8.5% to 10-11%): the contention is below
  the VM.
- **The slow member is not on the commit path.** Its own region's writes, forwarded
  through the throttled replica, commit like everyone else's in both regimes; in a
  separate pair of runs it kept p50 at 5.7 ms for the 170 s before the disk switched, as
  the run without a quota did throughout, with no retransmission or hedge on any member
  in the latter. Every container issued the same write IOs and bytes (cgroup `io.stat`;
  the slow one equal to the others, the leader up to 2% fewer), so the quota adds no disk
  load. The simulator agrees: with five replicas on a 1 ms LAN, a follower that spends
  200 or 600 us per message leaves the other origins' p50 unchanged (9.8 ms against 9.8 ms
  with no slow member, `SimConfig::slow_cpu`); only its own entries slow down.
- **Why about two `fsync`s.** A write commits once the leader's reservation and a majority
  of records are durable; with group commit a record often waits for the `fsync` already
  in progress before its own, so a commit costs between one and two `fsync`s of the
  moment. The five replicas share one disk, so they also queue behind each other's.
- The 1.7 ms figure #195 compared against is this host's idle `fsync` (`env.txt`), not a
  commit p50: with no quota the same harness commits at 5.7 ms in the fast regime.

The data: `target/bench-etcd/wan-20261113-213208/` through `wan-20261113-220040/`, the
probes and `io.stat` samples in `target/eval-results/slow-follower-ab/`. The earlier
heterogeneous runs (PR #194, #203) show the same two regimes, switching mid-run under
one leader.

Since #209 `wan.sh` runs this probe in every run (`*-disk.txt`) and `summary.md` gives
each case the probe's `fsync` p50 and max over the latency's window and a disk regime:
`slow` from a p50 of 6 ms (`DISK_SLOW_MS`), between the 4.5-4.6 ms and 8.0-8.6 ms of the
table above, `fast` below, `mixed` when a case's runs fall on both sides. A comparison
holds only between rows of the same regime; `DATA_TMPFS=1` takes the disk out entirely,
at the cost of durability.

### A member that gets faster (`wan.sh speedup`, #206)

Paper §5.3 footnote 7 suggests measuring a non-leader again when its conditions change,
rather than only once its measurement goes stale (`STALE_HOLDS`, eight holds: 960 s at
`--placement-periods 30000,120000`). The cheap trigger considered: a tied member's own
commit tail, which it reports in its probes as a follower, drops clearly below what it
was. The `speedup` scenario tests whether that signal exists: `heterogeneous` with the
first leader (ap-ne) on a quota that is lifted at 150 s.

Three back-to-back 300 s runs at `ebc4f34` (`SYSTEMS=quepaxa SLOW_CPUS=0.2`; 0.5 CPU, the
default, leaves a leader indistinguishable even while it leads, see above). Commit p90 in
ms of ap-ne's own writes as a follower under the quota (the last tenure before 150 s) and
after the lift, against the other regions' over the same windows:

| Run | Disk regime | ap-ne quota, p90 | others, p90 | ap-ne lifted, p90 | others, p90 | ap-ne leading under quota, p90 / p99 | others leading, p90 / p99 |
|---|---|---|---|---|---|---|---|
| 1 | fast | 8.9 | 8.6-8.7 | 8.7 | 8.5-8.8 | 9.4 / 18.9 | 8.4-8.8 / 10.6-12.2 (one 186) |
| 2 | fast, slow from about 150 s | 8.6 | 8.4-8.7 | 27.3 | 27.0-27.9 | 9.3 / 16.6 | 8.4-8.8 / 10.8-12.2 (one 178) |
| 3 | slow | 29.5 | 29.3-30.1 | 29.5 | 29.5-29.9 | 27.4 / 33.4 | 28.2-32.0 / 33.5-226 |

- **There is no signal to trigger on.** A throttled follower's own writes commit like
  everyone else's (within 0.3 ms of p90), before and after the lift: the commit path runs
  through the leader and the disk, and a follower's CPU is off it (as "A slow member"
  found at 0.5 CPU). The tail it reports cannot drop when it gets faster, because it never
  rose while it was slow. Only a tenure measures a member's speed as a leader.
- **The disk moves every tail at once**, 8.6 to 27.7 ms in run 2: a trigger on absolute
  tails would re-explore on every regime switch, and one relative to the other members'
  would never fire.
- **Nothing to gain in this scenario anyway.** At 0.2 CPU the throttled leader's p90 was
  about 10% above the others' in the fast regime and indistinguishable in the slow one;
  once lifted, every member is alike, so the lead going back to it (`back_to_fast_s`: not
  within the run in any of the three) would gain nothing. Run 3 shows the existing
  mechanism's bound instead: ap-ne's first tenure (25 s) did not span the sustained
  period, so it was explored again at 116 s and rotation demoted it after 13 s.

So the age bound stays: a measurement is retaken once it is older than eight holds, and no
cheaper trigger is added (the policy and its Verus port are unchanged). A signal that
could see a follower's speed as a leader would need the follower to time its own leader
work (say, how long it takes to process a proposal), which it does not do as a follower.

The data: `target/bench-etcd/wan-20261114-094533/` (the three runs) and
`wan-20261114-004901/` (one earlier run in the fast regime: ap-ne as follower p90 10.2 ms
against 9.7 ms for us-east; as leader 14.6 against 9.7-9.8).

## Not measured

- No comparison against the Go reference implementation: running the Go code was out of
  scope of the research (it was read, not built). The workload above mirrors its shape so
  a comparison can be added on the same machine.
- No deployment on real regions: the cross-region comparison above emulates the links on
  one host.

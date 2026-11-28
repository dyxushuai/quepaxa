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

### Apply off the core thread (#213)

The core thread keeps the `Replica` and hands each turn's commits, proposal registrations,
released reads and snapshot requests to an apply thread through one bounded FIFO (64
batches). The apply thread owns the KV state and answers clients
([ADR 0005](adr/0005-kv-tokio-runtime-threads.md)).

Same layout and workload as above, peers on 17100-17102 and clients on 17200-17202. Before
is `6f92f76`, after is this change. Runs were interleaved, three 15 s runs per point, the
whole matrix under `flock target/ops/bench.lock`, started at a 1-minute load of 2.65. The
load later rose to 6-10 from other work on the host. CPU is a share of one CPU, averaged
over the three replicas. Measured 2026-11-14.

| Offered rate | Throughput | p50 | p90 | p99 | Core thread CPU | Apply thread CPU | Process CPU |
|---|---|---|---|---|---|---|---|
| 10,000/s | 10,020 → 10,020 | 207 → 217 µs | 0.51 → 0.37 ms | 0.94 → 0.85 ms | 9.0 → 8.4% | 1.8% | 27 → 28% |
| 200,000/s | 199,998 → 199,998 | 590 → 610 µs | 2.5 → 1.2 ms | 17.7 → 10.7 ms | 40 → 20% | 21% | 86 → 85% |
| 250,000/s | 250,041 → 250,040 | 680 → 697 µs | 9.0 → 2.0 ms | 35.9 → 17.6 ms | 47 → 23% | 28% | 97 → 98% |
| 800,000/s | 800,168 → 800,169 | 610 → 164 ms | 4.33 → 0.97 s | 4.42 → 1.61 s | 87 → 28% | 83% | 137 → 161% |

- **Saturation:** much better. The core thread no longer runs apply, so it stays well short of a full core.
  Under overload the apply work moves onto the second CPU, and p99 drops from 4.4 s to 1.6 s.
- **Tail at 200k-250k/s:** p90 is a half to a quarter of before, and p99 is about half.
- **Low load costs latency:** p50 at 10k/s is higher. Six more interleaved pairs at a load of
  at most 2.6, half with after first, give 192.5 → 211 µs (ranges 189-208 vs 207-223 µs) and
  an unchanged p99 (852 vs 847 µs). The likely cause is one more cross-thread wake-up per
  commit: process context switches rose from 35k/s to 41k/s. On a disk, the commit path
  is dominated by fsync (milliseconds), so the 18 µs is a cost only in-memory setups see.
- The single-owner alternative (all of a replica on one thread) used about 30% less CPU,
  but lost 15% of saturation throughput with two CPUs per replica. See ADR 0005.

`wan.sh calm` (QuePaxa only, five regions, 1,000 writes/s, one run per side, latencies in
ms). Every case ran in the fast disk regime (fsync p50 2.5-2.9 ms). With one run per side,
this shows only that WAN latency holds:

| Case | Build | p50 | p99 | max | Disk regime |
|---|---|---|---|---|---|
| calm | before | 444 | 654 | 770 | fast |
| calm | after | 438 | 649 | 758 | fast |
| calm-best | before | 204 | 329 | 398 | fast |
| calm-best | after | 192 | 316 | 365 | fast |

Raw data and scripts are kept outside the repo.

### Leader egress with large commands (#172, before batch IDs)

The baseline that issue #172's phases are measured against. Today the leader sends every
slot value twice to each of the n−1 others: once in `RecordRequest` (89 + V bytes) and once
in `Decided` (85 + V bytes), so (n−1)(174 + 2V) bytes per slot, where V is the slot value
(4 bytes plus 25 + payload per entry). `replica::egress_tests::leader_egress_bytes_per_slot`
asserts that exactly, for n = 3 and 5 with 17 B and 64 KiB payloads. For n = 3 and one
64 KiB entry it is 262,608 bytes. Later phases change that test: Phase 1 (`Decided`
names the value by key, 80 bytes) makes it (n−1)(169 + V), 131,468 bytes for that case.

The same layout as #155: the 16 vCPU VM above, three replicas pinned to CPUs 0-1, 2-3 and
4-5, kv-bench on 8-15, data on tmpfs, peers on 37100-37102 and clients on 37200-37202.
`run-local.sh` options: `KV_SIZE` (17 B, 4 KiB, 64 KiB commands, `kv-bench --size`),
`KV_GETS=0` (all PUTs, so every command takes a slot), and with `KV_EGRESS=1gbit`, each
replica's egress capped at 1 Gbit/s. The cap works like this: each replica runs in its own
cgroup scope, nft marks its packets and an HTB class per mark on `lo` limits them. That
needs root (`tc`, `nft`). Clients write to all three replicas, so two thirds of the
commands reach the leader as follower forwards.

Each point uses a fresh cluster and 10 s of load. Capacity is the highest offered rate that
was sustained: no unanswered, failed or missed requests, throughput at least 97% of
offered, and p99 at most 100 ms. The ramp raises the rate ×1.5 per step from 50k/s, 4k/s
or 250/s, then tries the geometric midpoint once. There are three runs per point, medians
shown, run to run range in brackets. The overload point offers 2× capacity once per run.
Fixed rates are 10k/s, 2k/s and 200/s. Leader egress per slot is the leader's `STATS`
`EGRESS` bytes (framing included) over its `slots_decided`. The whole matrix ran under
`flock target/ops/bench.lock`, starting at a 1-minute load of 2.2. Measured 2026-11-20.

| Egress cap | Command | Capacity | Leader egress per slot | At 2× capacity: throughput, unanswered | Fixed rate p50 / p99 |
|---|---|---|---|---|---|
| none | 17 B | 206,675/s (168,750-253,125) | 2,653 B | 413,409/s, 0 | 0.25 / 1.41 ms |
| none | 4 KiB | 24,801/s (all three) | 43,628 B | 49,481/s, 0 | 0.32 / 1.76 ms |
| none | 64 KiB | 2,322/s (1,032-3,483) | 270,716 B | 4,459/s, 0 | 2.36 / 4.24 ms |
| 1 Gbit/s | 17 B | 206,675/s (137,783-206,675) | 912 B | 413,409/s, 0 | 0.27 / 1.12 ms |
| 1 Gbit/s | 4 KiB | 7,348/s (all three) | 31,855 B | 3,097/s, 42,125 | 0.45 / 1.40 ms |
| 1 Gbit/s | 64 KiB | 375/s (all three) | 263,321 B | 134/s, 6,197 | 5.12 / 12.35 ms |

- **Go for Phase 2: under the cap, leader egress is the limit for large commands.** Capacity
  falls 3.4× at 4 KiB and 6.2× at 64 KiB against no cap, while 17 B does not move. At
  capacity the leader sends 122.6 MB/s at 4 KiB (98% of 1 Gbit/s) and 98.5 MB/s at 64 KiB
  (79%; the next ramp step would need about 148 MB/s). The 64 KiB slots carry one command
  each, at 263 KB per slot, close to the formula's 262.6 KB.
- **17 B is bounded by the p99 rule, not by throughput.** At 2× capacity every request still
  completes, so the ramp stops where p99 passes 100 ms.
- **Overload collapses under the cap.** At 2× capacity, throughput drops to 3.1k/s at 4 KiB
  and 134/s at 64 KiB. Followers hedge (44,863 and 6,804 times) and the leader retransmits.
  Without the cap, 4 KiB and 64 KiB hold at 2×. Fixed in #231 (below).
- Uncapped 64 KiB capacity varies a lot between runs (1,032-3,483/s). With no cap the leader
  sent 413 MB/s at 4 KiB and 609 MB/s at 64 KiB, so CPU and memory copies limit there, not
  the link.
- Per slot, egress without the cap is larger at 17 B and 4 KiB than with it because slots
  are batched differently, so compare egress per slot only at one size and cap.

### Overload on a capped link (#231)

Same layout and 1 Gbit/s per-replica egress cap as above, with `Decided` already carrying
keys only (Phase 1 of #172, where 1 Gbit/s line rate is ~15.2k/s at 4 KiB and ~952/s at
64 KiB for three replicas). Offered rates are ~2× capped capacity (27,000/s at 4 KiB,
1,800/s at 64 KiB), 10 s per run, interleaved before (`3286eb9`) and after (`3fd34ea`),
three runs per point under `flock target/ops/bench.lock` at 1-minute load 1.4-2.2.
Measured 2026-11-23. "In-window" counts completions that finished within the 10 s offered
window (excluding requests that drained during the 5 s tail).

| Command | Build | In-window median (runs) | Unanswered | Busy | p50 (range) | p99 (range) | Retransmits (cluster) |
|---|---|---|---|---|---|---|---|
| 4 KiB @ 27k/s | before | 3,279/s (700-3,293) | 46,602-49,456 | 0 | 1.15-5.04 s | 12.4-13.9 s | 1,697-6,912 |
| 4 KiB @ 27k/s | after | 14,413/s (13,771-15,098) | 0 | 35,145-43,416 | 0.50-0.69 s | 1.10-2.34 s | 38-63 |
| 64 KiB @ 1.8k/s | before | 145/s (141-185) | 15,112-16,543 | 0 | 3.79-8.36 s | 4.79-11.4 s | 1,563-4,488 |
| 64 KiB @ 1.8k/s | after | 933/s (619-943) | 0 | 4,167-7,559 | 0.64-0.90 s | 1.82-3.51 s | 41-118 |

Six pieces together held throughput at the link's capacity with zero unanswered requests:
RTT-floored exponential backoff on record resends (with slow resent slots still raising
the floor), numbered forwards with coalesced `ForwardAck`s timed from their own
acknowledgement delay, holding hedges while a target still decides (up to 4 · `hedge_max`),
updating reply delay R when a suspected leader speaks, and byte-bounded admission control
in `kv-tokio` (`max_batch_bytes × max_in_flight_slots × 2`, 16 MiB default) paired with
connection backoff on `Busy` in `kv-bench`.

Raw data and scripts are kept outside the repo.

### Batch IDs: LAN and WAN A/B (#172 Phase 3)

Interleaved A/B comparison of `Config::inline_batch_bytes = u32::MAX` (`off`, Phase 1 inline
forwarding with key-only `Decided`) against `1024` bytes (`on`, Phase 2 submitter-driven
`Data` dissemination and 33-byte `Ref` items) on `a790ff2`, measured 2026-11-24 under
`flock target/ops/bench.lock`. `replica::egress_tests::leader_egress_bytes_per_slot_with_references`
asserts the exact Phase 2 wire formula: for a follower-originated chunk above `1024` bytes,
leader egress per slot drops to `(n - 1)(169 + 37) = (n - 1) · 206` bytes (`412` B at `n = 3`,
`824` B at `n = 5`) regardless of payload size, with zero fetches in order.

#### LAN (10 s per step, all PUTs, 3 interleaved runs per point)

Same layout and ramp rule as the Phase 0 baseline above (sustained: zero errors, unanswered,
missed, busy or shed requests, throughput ≥ 97% of offered, p99 ≤ 100 ms; `n = 3` pinned to
CPUs `0-1 2-3 4-5` with `kv-bench` on `8-15` and 24 connections; `n = 5` pinned to
`0-1 2-3 4-5 6-7 8-9` with `kv-bench` on `10-15` and 25 connections). Fixed rates are 10k/s
(17 B), 2k/s (4 KiB) and 200/s (64 KiB). Medians shown, run-to-run range in parentheses.

| Suite | Command | Mode | Capacity (runs) | On / off | Leader egress/slot | Follower egress/slot | Fixed p50 (ms) | Fixed p99 (ms) | `data_sent` (cap) | `fetches` (cap / fixed) |
|---|---|---|---|---|---|---|---|---|---|---|
| `n=3`, tmpfs, 1 Gbit/s | 17 B | off | 183,711/s (122k–225k) | – | 1,171 B | 580 B | 0.29 (0.26–0.40) | 1.55 (1.04–2.98) | 0 | 0 / 0 |
| | 17 B | on | 225,000/s (184k–225k) | 1.22× | 1,937 B | 674 B | 0.28 (0.27–0.69) | 1.27 (1.16–6.64) | 30 | 0 / 0 |
| | 4 KiB | off | 15,000/s (all three) | – | 266,142 B | 45,070 B | 0.36 (0.35–0.38) | 1.41 (1.33–1.49) | 0 | 0 / 0 |
| | 4 KiB | on | 24,494/s (14.1k–36.7k) | 1.63× | 8,018 B | 7,869 B | 0.30 (0.30–0.32) | 1.35 (1.26–1.49) | 325,498 | 7,316 / 862 |
| | 64 KiB | off | 734/s (424–734) | – | 133,149 B | 22,233 B | 3.87 (3.77–3.95) | 7.51 (7.34–7.87) | 0 | 0 / 0 |
| | 64 KiB | on | 1,500/s (750–1,837) | 2.04× | 57,235 B | 46,300 B | 3.20 (3.09–3.21) | 5.65 (5.61–5.67) | 19,990 | 1,507 / 438 |
| `n=3`, tmpfs, uncapped | 17 B | off | 225,000/s (all three) | – | 1,978 B | 702 B | 0.25 (0.25–0.28) | 1.07 (1.06–1.35) | 0 | 0 / 0 |
| | 17 B | on | 225,000/s (all three) | 1.00× | 1,996 B | 707 B | 0.26 (0.25–0.27) | 1.20 (1.08–1.29) | 32 | 0 / 0 |
| | 4 KiB | off | 36,742/s (14.1k–55.1k) | – | 31,960 B | 5,442 B | 0.30 (0.30–0.35) | 1.64 (1.59–1.81) | 0 | 0 / 0 |
| | 4 KiB | on | 36,742/s (all three) | 1.00× | 11,332 B | 11,392 B | 0.29 (0.29–0.32) | 1.49 (1.44–1.63) | 489,212 | 3,390 / 184 |
| `n=5`, tmpfs, 1 Gbit/s | 17 B | off | 168,750/s (all three) | – | 3,260 B | 389 B | 0.35 (0.34–0.35) | 1.38 (1.10–1.53) | 0 | 0 / 0 |
| | 17 B | on | 168,750/s (all three) | 1.00× | 3,265 B | 379 B | 0.34 (0.34–0.35) | 1.10 (1.05–1.45) | 28 | 2 / 0 |
| | 4 KiB | off | 7,500/s (3.5k–7.5k) | – | 159,794 B | 8,202 B | 0.61 (0.61–0.64) | 1.69 (1.66–1.97) | 0 | 0 / 0 |
| | 4 KiB | on | 10,606/s (10.6k–18.4k) | 1.41× | 6,821 B | 4,583 B | 0.43 (0.43–0.44) | 1.43 (1.21–1.67) | 338,976 | 34,574 / 12,357 |
| | 64 KiB | off | 367/s (all three) | – | 267,052 B | 13,378 B | 6.20 (6.14–6.25) | 16.53 (15.81–17.11) | 0 | 0 / 0 |
| | 64 KiB | on | 848/s (all three) | 2.31× | 104,341 B | 54,452 B | 4.42 (4.38–4.43) | 8.76 (8.54–8.82) | 27,060 | 5,932 / 2,797 |
| `n=3`, disk, 1 Gbit/s | 4 KiB | off | 4,000/s (2.8k–14.7k) | – | 50,684 B | 8,617 B | 5.13 (5.11–18.09) | 36.24 (24.12–49.80) | 0 | 0 / 0 |
| | 4 KiB | on | 7,500/s (5.3k–27.6k) | 1.88× | 46,341 B | 48,746 B | 5.08 (5.06–20.17) | 10.74 (10.49–36.89) | 100,380 | 56 / 454 |
| | 64 KiB | off | 750/s (176–750) | – | 147,592 B | 24,676 B | 5.92 (5.74–15.14) | 30.12 (11.20–106.29) | 0 | 0 / 0 |
| | 64 KiB | on | 1,200/s (848–2,204) | 1.60× | 132,298 B | 131,107 B | 5.62 (5.46–13.97) | 29.64 (9.63–311.21) | 15,988 | 914 / 359 |

- **Acceptance bar met.** Against the Phase 0 baseline at `n = 3`, 1 Gbit/s (`7,348/s` at
  4 KiB and `375/s` at 64 KiB), combined Phase 1 + Phase 2 delivers **3.33×** at 4 KiB
  (`24,494/s`, up to `36,742/s` = 5.0× without snapshot interference) and **4.00×** at
  64 KiB (`1,500/s`, up to `1,837/s` = 4.9×). Against Phase 1 alone, references add 2.04×
  at `n = 3` 64 KiB, 2.31× at `n = 5` 64 KiB, and 1.60–1.88× on disk (run 1 in the fast
  disk regime reached `27,556/s` vs `14,696/s` at 4 KiB and `2,204/s` vs `750/s` at 64 KiB;
  runs 2–3 ran in the shared disk's slow regime). Fixed-rate p50 improves across all large
  cases (−1% to −30%), and 17 B stays within 4% on every suite.
- **Why capped large-command runs sit below the theoretical 3× / 5× ceiling.** Two
  mechanisms account for the gap:
  1. `disseminate_chunk` iterates `schedule.everyone()` in `NodeId` order, so followers
     queue `Data` and `Forward(Ref)` to leader `0` *before* queuing `Data` to higher-ID
     peers. Leader `0`'s compact `RecordRequest(Ref)` therefore beats the origin's direct
     `Data` on ~10–30% of chunks (`7,316` fetches at `n = 3` 4 KiB; `34,574` at `n = 5`
     4 KiB; `5,932` at `n = 5` 64 KiB), sending the payload back through the leader's egress
     via `fetches_served` (notice `n = 5` 64 KiB leader egress `104 KB/slot` vs follower
     `54 KB/slot`). Reordering dissemination to send to non-target peers before the target
     is a small follow-up.
  2. At 4 KiB with random keys, smaller batches advance `slots_decided` past the default
     `--snapshot-every 10000` within 10 s and trigger background encoding of hundreds of MB
     of KV state (`n = 3` 4 KiB run 2 stayed at `9,999` slots and sustained `36,742/s`,
     2.45× over Phase 1, whereas run 3 crossed 70,735 slots at 20k/s and tripped `Busy`
     during snapshot encoding).

#### WAN (`examples/kv-tokio/bench/etcd/wan.sh`, QuePaxa only, 5 regions)

Five emulated regions (`us-east`, `us-west`, `eu-west`, `ap-ne`, `sa-east`), 200 puts/s per
region (1,000/s total), 40 s per run (fault 10–25 s), on host ext4 disk, three interleaved
runs per point (`SIZE=17` and `SIZE=4096`, `INLINE_BATCH_BYTES=4294967295` vs `1024`).

| Size | Case | Mode | p50 (ms) | p99 (ms) | max (ms) | Longest gap (ms) | Errors+Unans | `data_sent` | `fetches` | `parked` | Disk `fsync` p50 |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 17 B | calm | off | 394.3 (394–397) | 607.7 (593–616) | 794.2 | 113 (107–157) | 0 | 0 | 0 | 0 | 6.3 ms |
| | calm | on | 424.5 (404–431) | 658.0 (643–1,160) | 772.7 | 271 (161–728) | 0 | 0 | 0 | 0 | 6.5 ms |
| | calm-best | off | 181.3 (180–182) | 304.3 (300–320) | 432.7 | 95 (83–179) | 0 | 0 | 0 | 0 | 6.7 ms |
| | calm-best | on | 182.2 (182–194) | 312.2 (304–475) | 424.9 | 181 (87–277) | 0 | 0 | 0 | 0 | 6.8 ms |
| | jitter | off | 452.9 (446–475) | 984.1 (882–1,138) | 1,345.2 | 663 (578–738) | 0 | 0 | 0 | 0 | 5.7 ms |
| | jitter | on | 456.7 (457–469) | 897.6 (806–1,432) | 1,241.5 | 605 (443–1,181) | 0 | 0 (0–4) | 0 | 0 | 5.4 ms |
| | crash | off | 213.6 (192–226) | 1,035.8 (943–1,087) | 12,386.4 | 769 (767–773) | 0 | 0 | 0 | 0 | 6.8 ms |
| | crash | on | 207.3 (195–209) | 999.2 (854–1,241) | 12,380.5 | 781 (781–783) | 0 | 20 (20–28) | 0 | 0 | 6.6 ms |
| 4 KiB | calm | off | 226.9 (213–233) | 9,553.3 (7,872–10,909) | 12,586.4 | 1,192 (668–2,686) | 0 | 0 | 0 | 0 | 6.0 ms |
| | calm | on | 475.5 (474–508) | 1,946.1 (1,901–6,132) | 6,778.1 | 212 (205–371) | 0 | 126,152 | 743 (217–1,675) | 41 | 2.7 ms |
| | calm-best | off | 219.0 (212–222) | 854.5 (841–2,417) | 3,314.8 | 230 (198–238) | 0 | 0 | 0 | 0 | 2.9 ms |
| | calm-best | on | 217.8 (213–227) | 870.9 (802–1,906) | 1,383.6 | 352 (207–1,282) | 0 | 126,032 | 3,095 (2,103–4,402) | 257 | 3.4 ms |
| | slow | off | 217.8 (216–230) | 12,504.3 (9,472–12,974) | 13,540.4 | 1,325 (360–3,797) | 0 | 0 | 0 | 0 | 2.8 ms |
| | slow | on | 232.0 (210–234) | 5,034.5 (4,432–6,367) | 6,206.4 | 266 (153–311) | 0 | 126,384 | 5,601 (431–5,938) | 660 | 6.5 ms |
| | jitter | off | 214.5 (211–222) | 30,521.7 (22,965–33,892) | 31,913.7 | 4,922 (2,578–19,856) | 4,254 (1,463–4,911) | 0 | 0 | 0 | 6.6 ms |
| | jitter | on | 302.9 (275–691) | 6,497.9 (2,160–7,465) | 8,162.5 | 557 (481–882) | 0 | 126,376 | 12,875 (1,685–13,023) | 1,259 | 3.0 ms |
| | crash | off | 205.7 (187–258) | 11,539.3 (5,703–13,184) | 12,905.5 | 337 (272–1,810) | 0 | 0 | 0 | 0 | 3.0 ms |
| | crash | on | 215.2 (215–225) | 5,621.0 (2,946–7,576) | 8,457.3 | 928 (828–981) | 0 | 103,088 | 947 (137–1,100) | 115 | 2.9 ms |
| | partition | off | 218.1 (213–229) | 12,895.0 (7,128–12,968) | 13,634.7 | 2,285 (283–2,706) | 0 | 0 | 0 | 0 | 3.2 ms |
| | partition | on | 213.3 (205–214) | 2,672.3 (2,308–6,220) | 7,671.6 | 715 (439–864) | 0 | 112,708 | 5,396 (5,195–9,271) | 1,128 | 6.5 ms |

- **17 B on WAN stays inline.** At 200 puts/s per region, forwarded chunks stay below
  `1024` bytes (`data_sent = 0` in `calm` and `calm-best`, `0–28` during fault backlogs,
  `fetches = 0` everywhere), and p99 matches the existing baseline (`calm` 658 ms vs 655 ms,
  `calm-best` 312 ms vs 339 ms, `jitter` 898 ms vs 1,435 ms, `crash` 999 ms vs 974 ms).
- **4 KiB on WAN improves dramatically with references on.** At 1,000 × 4 KiB/s (4 MB/s
  raw writes), inline `off` pushed 16 MB/s through the far leader `ap-ne` over 180–300 ms
  RTT TCP links and wrote each payload twice to disk; in every `calm`, `slow`, `jitter`,
  `crash`, and `partition` run `off` overloaded `ap-ne`, stalled for 9.5–30.5 s at p99, and
  rotated leadership out to `us-east` (which is why `off`'s `calm` p50 settled at 227 ms and
  its `crash` gap was 337 ms—`ap-ne` was no longer leading when killed at 10 s). With
  references `on`, `ap-ne` held leadership cleanly throughout `calm` (p99 1,946 vs 9,553 ms,
  longest gap 212 vs 1,192 ms), `jitter` dropped from 4,254 errors/unanswered and 30.5 s p99
  to **0 errors** and 6.5 s p99, `slow` and `partition` p99 dropped 2.5–4.8×, and `crash`
  failed over cleanly from `ap-ne` in 928 ms with 2.1× lower p99. On `calm-best`, p50 and
  p99 were equal (218 vs 219 ms; 871 vs 855 ms) with 2.4× lower max (1,384 vs 3,315 ms),
  while the `ap-ne ↔ sa-east` triangle-inequality tie (300 ms direct = 180 + 120 ms via
  `us-east`) produced `3,095` fetches.
- **Default decision.** Because `1024` bytes wins decisively on both bandwidth-limited LAN
  and large-command WAN while leaving small-command WAN inline with zero fetches,
  `Config::inline_batch_bytes` now defaults to `1024` (`u32::MAX` disables it per replica;
  see `crates/quepaxa/docs/tuning.md`). Raw data and scripts are kept outside the repo.

### Online `in_flight_slots` tuning (#175)

Three-run comparison of `Config::tune_in_flight_slots = true` (`auto`, ceiling
`--max-in-flight-slots 16`) against static settings on LAN (`run-local.sh`, 10 s, all PUTs,
`n = 3` pinned to `0-1 2-3 4-5`, `kv-bench` on `6-9`) and five-region WAN (`wan.sh load`,
20 s per rate, best-placed leader `us-east`, every run in the `fast` disk regime at 2.6–4.7 ms
probe p50), measured 2026-11-27 on `feat/175-online-w-tuner` (stacked on `#251`, `#252`,
`#258`). Medians over 3 runs, min–max ranges in parentheses, latencies in ms.

#### LAN (3 replicas, 10 s per run)

| Workload | Setting | p50 (ms) | p90 (ms) | p99 (ms) | Retransmits | Final `W` | Notes |
|---|---|---|---|---|---|---|---|
| disk, 17 B, 200k/s | static `W=1` | 7.29 (7.21–7.72) | 9.90 (9.68–12.34) | 13.15 (12.64–352.02) | 0 (0–3) | 1 | 1/3 host-disk spike |
| | static `W=8` | 7.46 (7.46–7.48) | 10.93 (10.88–11.00) | 25.59 (18.92–26.33) | 0 (0–7) | 8 | default ceiling |
| | `auto` (`max_w=16`) | **6.91** (6.49–6.99) | **9.23** (8.71–9.69) | 62.77 (11.71–197.46) | 1 (0–2) | 1 (3/3) | 2/3 host-disk spikes |
| disk, 4 KiB, 10k/s | static `W=1` | 6.79 (6.28–6.97) | 1,373.9 (8.56–2,805.7) | 2,678.9 (11.48–3,716.6) | 1 (0–5) | 1 | `busy=10,663` in run 1 |
| | static `W=8` | 6.51 (6.51–6.57) | 9.80 (9.36–10.03) | 13.32 (12.18–47.04) | 0 (0–8) | 8 | |
| | `auto` (`max_w=16`) | **5.76** (5.65–6.06) | **7.89** (7.60–8.20) | **11.15** (10.45–37.33) | 0 (0–1) | 1 (3/3) | `busy=0`, beats `W=1` and `W=8` |
| tmpfs, 17 B, 250k/s | static `W=2` | 0.71 (0.67–0.77) | 11.04 (5.20–39.00) | 82.63 (59.37–96.16) | 0 | 2 | |
| | static `W=8` | 0.94 (0.94–0.95) | 31.95 (30.46–33.18) | 95.56 (87.56–96.59) | 0 | 8 | |
| | `auto` (`max_w=16`) | **0.64** (0.63–0.64) | **1.37** (1.36–1.43) | **59.90** (57.37–63.76) | 0 | 1 (3/3) | 8×–23× lower p90 |

#### WAN (5 regions, `wan.sh load`, 20 s per rate, all `fast` disk regime)

| Workload | Setting | p50 (ms) | p99 (ms) | max (ms) | Retransmits | Fetches | Final `W` | Disk p50 |
|---|---|---|---|---|---|---|---|---|
| 17 B, 5k/s | static `W=4` | 218.0 (211.2–219.2) | 346.6 (344.9–589.3) | 516.9 | 0 | 0 | 4 | 2.6 ms |
| | static `W=8` | 214.7 (214.0–215.2) | 350.6 (343.3–641.8) | 540.0 | 0 | 0 | 8 | 3.3 ms |
| | `auto` (`max_w=16`) | 223.3 (221.2–224.1) | 353.4 (**348.8–356.5**) | 545.0 | 0 | 0 | 1 (3/3) | 3.2 ms |
| 17 B, 25k/s | static `W=4` | 233.0 (231.9–234.1) | 506.8 (467.3–972.7) | 971.5 | 0 | 0 | 4 | 3.9 ms |
| | static `W=8` | 231.1 (225.3–231.7) | 874.4 (512.2–912.8) | 1,156.4 | 0 | 0 | 8 | 3.6 ms |
| | `auto` (`max_w=16`) | 236.4 (235.2–236.7) | **495.1** (**442.1–525.3**) | **925.9** | 0 | 0 | 1 (3/3) | 3.8 ms |
| 4 KiB, 2k/s | static `W=8` | 219.6 (217.3–338.3) | 1,465.2 (1,238.8–3,827.8) | 2,013.0 | 1 (0–11) | 69 (65–844) | 8 | 3.0 ms |
| | static `W=16` | 383.4 (353.6–716.0) | 4,306.6 (4,264.7–5,777.8) | 6,383.1 | 23 (19–76) | 850 (58–968) | 16 | 3.5 ms |
| | `auto` (`max_w=16`) | 222.9 (220.5–226.9) | **1,395.1** (1,231.2–2,122.1) | **1,947.7** | **0** (0–0) | **66** (49–1,439) | 16 (3/3) | 3.3 ms |

- **No single static `W` wins everywhere.** Static `W=1` collapses under 4 KiB bursts (2.68 s
  p99 on LAN disk when a stall hits; >12 s and errors on WAN 4 KiB). Static `W=8` suffers a
  tail cliff on small-command WAN (`874 ms` p99 at 25k/s, and up to `642–648 ms` at 5k/s when
  farthest follower `ap-ne` falls behind the fast quorum). Static `W=16` floods cold-start
  WAN 4 KiB into 19–76 retransmits and `4.31 s` p99.
- **Online tuning matches or beats the best static setting across regimes.** On LAN it holds
  `W=1` (beating static `W=8` on disk 4 KiB and cutting tmpfs p90 by 8×–23×), while stepping
  up momentarily if a disk stall saturates a batch. On small-command WAN it avoids both the
  `W=8` 25k/s degradation (`495 ms` vs `874 ms` p99) and the `ap-ne` 5k/s cliff (`349–357 ms`
  tight range). On 4 KiB WAN it ramps cleanly to `W=16` with **0 retransmits** across all
  three runs, beating both static `W=8` (`1,465 ms`) and static `W=16` (`4,307 ms`).
- **Default stays off for now:** `Config::tune_in_flight_slots` defaults to `false` so
  existing deployments keep deterministic static concurrency until broader multi-host testing
  (#159) confirms the same behavior outside a shared virtual disk.

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

## Against Openraft (across regions, #156)

Interleaved five-region WAN comparison of `kv-tokio` (QuePaxa) against a standalone
`openraft 0.10.1` KV server (`examples/kv-tokio/bench/openraft/`, isolated from the root
workspace so core dependencies stay untouched). Both systems run five containers pinned to
CPUs `0-1 2-3 4-5 6-7 8-9` across the `wan_leaders` five-region RTT matrix (70-300 ms via
`tc netem`), persist to the host ext4 disk with group-commit `sync_data()` on a CRC32-framed
append-only WAL (`records.log` vs `raft.log`), use `mimalloc`, and serve the same etcd v3 KV
gRPC API driven by five open-loop `kv-bench --protocol etcd` generators (200 puts/s per
region, 1,000 puts/s total, 40 s runs, fault at 10 s, heal at 25 s). Openraft enables
Pre-Vote (`enable_pre_vote = Some(true)`), pipelined bidirectional `StreamAppend`, dedicated
heartbeat HTTP/2 channels, atomic durable snapshots every 10,000 entries, and follower-to-leader
`ForwardPut` with immediate cancellation on local leader change and a 7 s timeout (matching
etcd).

```sh
SYSTEMS="quepaxa openraft openraft-tuned" INTERLEAVE=1 REPEATS=3 \
  examples/kv-tokio/bench/etcd/wan.sh
```

**Timers.** QuePaxa runs with its WAN timers (`--retransmit-ms 400 --hedge-max-ms 3000`).
Openraft 0.10.1 enforces `C::timeout(heartbeat_interval)` on every heartbeat RPC (so
`heartbeat_interval` must exceed the 300 ms maximum WAN RTT) and sets each follower's
`leader_lease` to `election_timeout_max` (so follower elections fire after
`election_timeout_max + [election_timeout_min, election_timeout_max)`). To match etcd's and
`etcd-tuned`'s failover windows fairly rather than penalizing Openraft with library LAN
defaults (`50 / 150-300 ms`, which cannot hold a leader across 300 ms RTTs) or overly wide
leases:

- `openraft`: heartbeat 400 ms, election 1,000-1,500 ms (2.5-3.0 s nominal failover).
- `openraft-tuned`: heartbeat 1,000 ms, election 2,000-3,000 ms (5.0-6.0 s nominal failover).

Measured 2026-11-24 at `a790ff2` on the 16-vCPU VM above (idle 512-byte `fsync`: 1.69 ms
mean, starting 1-minute load 5.91, all 54 trials interleaved per scenario and repeat under
`flock target/ops/bench.lock`). Raw data and `summary.md` are kept outside the repo under
`~/.local/share/quepaxa-eval/openraft-156/full/`. Medians of three runs with min-max ranges
in parentheses, latencies in ms.

With the leader's region alive, across all five regions' writes:

| Leader's region | System | p50 | p99 | max | Longest gap | Errors | Disk regime (probe p50) |
|---|---|---|---|---|---|---|---|
| far (`ap-ne`), no fault | QuePaxa | 435 (434–438) | 648 (648–649) | 814 (765–899) | 184 (184–186) | 0 | fast (2.5–2.8 ms) |
| | openraft | 365 (365–366) | 489 (489–554) | 842 (776–868) | 116 (114–370) | 0 | fast (3.0–3.5 ms) |
| | openraft-tuned | 365 (365–366) | 488 (488–533) | 872 (871–1,096) | 52 (50–61) | 0 | fast (3.1–3.3 ms) |
| best (`us-east`), no fault | QuePaxa | 200 (185–205) | 323 (315–365) | 450 (372–478) | 182 (84–223) | 0 | fast (2.6–3.2 ms) |
| | openraft | 173 (165–185) | 410 (277–543) | 624 (431–864) | 340 (170–439) | 0 | mixed (3.1–7.5 ms) |
| | openraft-tuned | 174 (166–205) | 382 (376–998) | 661 (608–1,207) | 392 (366–666) | 0 | mixed (3.1–7.0 ms) |
| far, +100 ms on its links | QuePaxa | 499 (486–503) | 914 (831–918) | 963 (907–964) | 288 (242–289) | 0 | fast (2.6–2.9 ms) |
| | openraft | 416 (415–2,789) | 767 (732–13,605) | 1,122 (1,033–14,530) | 471 (353–9,330) | 0 (0–9,931) | mixed (3.2–70.7 ms) |
| | openraft-tuned | 416 (415–422) | 720 (695–724) | 1,138 (1,001–1,142) | 172 (122–315) | 0 | mixed (3.2–6.9 ms) |
| far, jitter (σ 50 ms) & 1% loss | QuePaxa | 460 (450–472) | 891 (877–910) | 1,144 (1,087–1,229) | 511 (467–535) | 0 | fast (2.6–4.7 ms) |
| | openraft | 621 (604–2,433) | 7,349 (6,880–7,785) | 7,842 (7,060–8,392) | 3,958 (1,386–7,185) | 2,939 (865–5,532) | mixed (3.0–54.9 ms) |
| | openraft-tuned | 747 (637–957) | 5,730 (5,021–6,455) | 6,173 (5,506–6,701) | 568 (539–986) | 0 | fast (2.9–3.4 ms) |

With the leader's region (`ap-ne`) crashed or cut off, across the four surviving regions'
writes (percentiles cover successful requests only; requests Openraft fails during elections
are counted in `Errors`, not in latency percentiles):

| Fault | System | Errors | Longest gap | p50 | p99 | max | Leader after | Disk regime |
|---|---|---|---|---|---|---|---|---|
| crash (`SIGKILL`) | QuePaxa | 0 | 714 (707–780) | 208 (190–209) | 1,092 (928–1,127) | 12,398 (12,354–12,417) | `us-east` (3/3) | mixed (2.7–6.4 ms) |
| | openraft | 3,035 (2,997–3,143) | 3,951 (3,921–3,979) | 287 (286–293) | 588 (573–590) | 869 (862–1,091) | `eu-west` (3/3) | mixed (3.0–6.0 ms) |
| | openraft-tuned | 4,896 (4,701–5,358) | 6,027 (5,991–6,846) | 204 (204–346) | 528 (527–662) | 873 (850–1,094) | `us-east` (2), `sa-east` (1) | fast (3.2–3.7 ms) |
| partition (15 s) | QuePaxa | 0 | 732 (608–754) | 211 (207–212) | 963 (865–1,076) | 12,415 (1,364–12,417) | `us-east` (3/3) | fast (2.5–3.0 ms) |
| | openraft | 5,188 (3,366–5,820) | 6,624 (6,366–7,552) | 286 (166–346) | 649 (593–2,264) | 3,160 (2,733–4,090) | `us-east`, `sa-east`, `us-west` | fast (3.3–3.8 ms) |
| | openraft-tuned | 5,367 (5,073–5,402) | 7,557 (6,825–8,662) | 295 (285–346) | 959 (582–2,178) | 1,379 (1,132–2,564) | `us-west`, `eu-west`, `sa-east` | fast (3.1–3.4 ms) |

What the tables show, and what they do not:

- **Steady state (`calm`, `calm-best`) and uniform `+100 ms` delay (`slow`): Openraft has lower
  steady-state latency.** With `ap-ne` leading, Openraft's local commit p50 is 185 ms (180 ms
  RTT to its second-nearest peer `us-east` plus ~5 ms `fsync`) and all-region p50/p99 is
  365/488-489 ms, against QuePaxa's 263 ms local p50 and 435/648 ms overall. Under `+100 ms`
  uniform leader delay (`slow`), `ap-ne` stays leader in all three systems and `openraft-tuned`
  commits at 416/720 ms with 0 errors versus QuePaxa's 499/914 ms. Openraft streams
  `AppendEntries` continuously over `StreamAppend` without filling a fixed 8-slot in-flight
  window, whereas QuePaxa's default `max_in_flight_slots = 8` fills in bursts on a 180 ms
  quorum RTT (visible in its 184 ms `calm` gap). In `calm-best`, Openraft's p50 stays lower
  (173 vs 200 ms), while its p99 (382-410 vs 323 ms) reflects both mixed disk spikes and
  40,000 single-entry Raft logs crossing its 10,000-entry snapshot threshold 3-4 times per
  run (QuePaxa batches many writes per slot and stays below its 10,000-slot snapshot
  threshold).
- **Host disk stalls affected two Openraft runs.** `openraft` run 3 of `slow` hit a shared-host
  disk stall (probe `fsync` p50 70.7 ms, max 6.79 s), which broke its 400 ms heartbeat and
  caused all 9,931 errors (runs 1-2 in the fast regime had 0 errors and p99 732-767 ms).
  `openraft` run 2 of `jitter` similarly hit a 54.9 ms disk p50 (5,532 errors), though runs 1
  and 3 in the fast disk regime (3.0-3.1 ms) still saw 865-2,939 errors and 6.9-7.3 s p99.
- **Jitter + 1% loss (`jitter`): QuePaxa cuts p99 by 6.4×–8.2× with 0 errors.** All three
  `quepaxa` and `openraft-tuned` runs stayed in the fast disk regime (2.6-4.7 ms, 21-39 TCP
  reorders). QuePaxa held p99 at 891 ms with 0 errors. `openraft-tuned` avoided spurious
  elections (0 errors) via its 1 s heartbeat / 3 s lease, yet sequential Raft replication
  head-of-line blocked behind 1% packet loss and netem jitter to a 5,730 ms p99 (similar to
  `etcd-tuned`'s 6,880 ms). Aggressive `openraft` (400 ms heartbeat) suffered both elections
  and head-of-line backlog (2,939 median errors, 7,349 ms p99).
- **Leader crash and partition: QuePaxa fails 0 requests and recovers 5.5×–10.3× faster.**
  QuePaxa rotated deterministically to `us-east` (next in `--leader-order`) in 714 ms
  (`crash`) and 732 ms (`partition`) with 0 errors. Openraft failed 3,035-5,367 writes across
  3,951-6,027 ms (`crash`) and 6,624-7,557 ms (`partition`) while waiting out its follower
  lease plus election timeout (and TCP/HTTP2 keepalive teardown under silent `iptables DROP`),
  and elected whichever survivor won the race (`eu-west`, `us-west`, `sa-east`, or `us-east`),
  moving post-fault p50 accordingly. Openraft's lower post-fault p99 (528-959 ms vs QuePaxa's
  963-1,092 ms) omits the 3k-5.4k failed requests, whereas QuePaxa's p99 includes every
  request delayed across the 714-732 ms failover.
- **QuePaxa `sa-east` 12.4 s max outlier on crash/partition.** In `crash` (3/3 runs) and
  `partition` (2/3 runs), 10-30 writes from `sa-east` (out of 32,000 surviving-region writes)
  forwarded right before `ap-ne` failed waited until the `4 · hedge_max` (12 s) upper bound
  introduced in #231 before hedging, completing at 12.3-12.4 s with 0 errors (`us-east`,
  `us-west`, and `eu-west` max stayed at 0.94-1.43 s).
- Single-host caveats apply identically to "Against etcd" above: emulated links, shared host
  CPUs and ext4 disk, and no bandwidth cap; hypotheses for real regions, not production claims.

## The paper's evaluation (§7, #174)

Figs. 6, 7, 8, 9 and 10 of the paper (Tennage et al., SOSP'23, §7.2–§7.6), rerun on this
host with `examples/kv-tokio/bench/paper-eval.sh`, which drives `run-local.sh` (LAN) and
`bench/etcd/wan.sh` (emulated WAN and Fig. 10).

```sh
examples/kv-tokio/bench/paper-eval.sh fig6-lan fig7 fig6-wan fig8 fig9 fig10
examples/kv-tokio/bench/paper-eval.sh --smoke fig6-lan fig7 fig6-wan fig8 fig9 fig10
```

Each figure takes the shared bench lock (`target/ops/bench.lock` in the main checkout) from
its build through its last run and writes raw outputs, `env.txt`/`host.txt` and `summary.md`
(`paper-summarize.sh`) under `~/.local/share/quepaxa-eval/paper-eval/`. Figs. 6, 7 and 10
below were taken on 2026-11-22 at `17e983d` and Figs. 8 and 9 on 2026-11-24 at `a790ff2` on
the 16-vCPU VM above (78 GiB RAM, Linux `7.0.14-17-pve`, tmpfs data, 17-byte `PUT` commands,
`--snapshot-every 1000000000000` so log compaction stays off as in the paper's prototype,
§7.1 footnote 8). Values are medians over three runs with min–max ranges in parentheses.

These are single-host measurements, not production claims. The paper ran each replica on its
own 16-vCPU EC2 `c4.4xlarge` and each submitter on a `c4.2xlarge`; here every replica and
every generator share one 16-vCPU VM, and unlocked work on the host raised 1-minute load to
6.2–13.7 during the matrix (recorded per figure).

### Fig. 6a–b: LAN, 3 replicas

Leader pinned to CPUs 0–1, followers sharing 2–11, `kv-bench` on 12–15; 60 s per offered
rate (`fig6-lan-20261122-124323`, start load 6.25).

| Offered (/s) | Throughput (/s) | p50 (ms) | p99 (ms) | Errors + unanswered + missed |
|---|---|---|---|---|
| 50,000 | 50,021 | 0.36 (0.35–0.38) | 1.35 (1.27–1.65) | 0 |
| 100,000 | 99,992 (99,991–99,992) | 0.46 (0.44–0.46) | 2.12 (1.55–2.66) | 0 |
| 200,000 | 200,015 (200,014–200,015) | 0.66 (0.60–0.67) | 5.27 (1.73–187) | 0 |
| 300,000 | 300,067 (300,066–300,067) | 1.21 (1.11–1.31) | 7.58 (2.71–36.67) | 0 |
| 400,000 | 351,929 (351,050–358,323) | 2.13 (1.98–2.35) | 27.66 (6.56–31.62) | 2,890,937 (2,506,851–2,941,789) |
| 500,000 | 338,330 (314,711–340,546) | 1.68 (1.54–1.79) | 6.80 (5.51–8.50) | 9,703,389 (9,564,483–11,112,692) |
| 600,000 | 298,274 (296,899–308,276) | 1.45 (1.32–1.51) | 4.39 (4.32–4.59) | 18,100,591 (17,496,716–18,176,060) |

- **Next to the paper:** §7.2 reports 584k cmd/s under 5.8 ms median on dedicated
  `c4.4xlarge` replicas with §6.1 submitter-driven batch dissemination (400k cmd/s for
  Multi-Paxos without it). Here `quepaxa` does not yet ship §6.1 (#172), and the 4-CPU
  `kv-bench` generator reaches its open-loop pacing ceiling near 350k/s (`missed` starts
  growing at 400k/s while median latency is still 2.13 ms).
- **Cost of compaction:** an earlier pass with `kv-tokio`'s default `--snapshot-every 10000`
  (`fig6-lan-snapshots-20261122-121713`, load 1.98) stayed clean at 100k/s (p50 0.63 ms,
  p99 67.9 ms) but jumped to p50 10.65 ms and p99 6.91 s at 200k/s as background snapshot
  encoding ran continuously, capping throughput near 295k/s.

### Fig. 7: LAN scalability, 3 to 13 replicas

Leader on CPUs 0–1, all followers sharing 2–11, `kv-bench` on 12–15; 20 s per ramp step
(100k/s steps plus one midpoint), sustained when throughput ≥ 97% of offered, 0
errors/unanswered/missed and p50 ≤ 5.8 ms (`fig7-20261122-124323`, start load 13.66).
`3-pipe1` pins `--max-in-flight-slots 1` (the paper disabled pipelining on the LAN).

| Replicas | Saturation throughput (/s) | p50 at it (ms) | p99 at it (ms) |
|---|---|---|---|
| 3 | 300,000 | 1.06 (0.93–1.08) | 20.93 (5.04–21.72) |
| 3 (`--max-in-flight-slots 1`) | 300,000 (300,000–350,000) | 0.78 (0.76–1.43) | 6.59 (2.11–23) |
| 5 | 300,000 | 1.29 (1.26–1.65) | 5.57 (4.45–10.64) |
| 7 | 300,000 | 2.03 (1.90–3.51) | 14.63 (9.77–26.72) |
| 9 | 300,000 | 4.08 (4.08–4.28) | 35.58 (21.44–73.86) |
| 11 | 200,000 (200,000–250,000) | 2.06 (1.75–5.45) | 12 (8.92–120) |
| 13 | 200,000 (150,000–200,000) | 5.12 (2.46–5.30) | 46.03 (6.91–110) |

- **Two regimes end the ramps:** for 3–7 replicas (and two of three `3-pipe1` runs), the
  350k/s step failed because `kv-bench` missed arrivals (`missed` 5k–63k) while median
  latency was still 1.3–1.8 ms, so 300k/s is the generator-limited floor rather than the
  5.8 ms knee. From 9 replicas on, `missed` was 0 and the cluster itself crossed 5.8 ms
  (9 replicas at 350k/s: p50 19.7–45.9 ms; 11 at 250k/s: 5.5–11.5 ms; 13 at 200k/s:
  5.1–27.0 ms, at 250k/s: 87–95 ms).
- **Next to the paper:** §7.3 reports 584k down to 467k cmd/s (−20%) from 3 to 13 replicas,
  each on its own machine with §6.1. Here leader fan-out plus 12 followers sharing 10 CPUs
  (and start load 13.7 from unlocked work) drops sustained throughput from 300k/s (at 9) to
  200k/s (150k–200k at 13).

### Fig. 6c–d: emulated WAN, 5 replicas, pipeline 10

Five Docker containers on CPUs `0-1 2-3 4-5 6-7 8-9`, five generators on `10-15`, `tc netem`
emulated links across the `wan_leaders` five-region matrix (70–300 ms RTT; best-placed
leader `us-east`), `--max-in-flight-slots 10`, WAN timers (`retransmit 400 ms`, `hedge_max
3 s`), 60 s per rate (`fig6-wan-20261122-124323`, load 12.95 at start, 12.58 at end).

| Offered (/s) | Throughput (/s) | p50 (ms) | p99 (ms) | Errors + unanswered |
|---|---|---|---|---|
| 1,000 | 997 (988–1,000) | 197 (197–199) | 321 (320–322) | 0 |
| 5,000 | 4,996 (4,983–5,002) | 214 (213–214) | 345 (343–451) | 0 |
| 10,000 | 10,000 (9,979–10,001) | 219 (218–219) | 1,482 (434–1,557) | 0 |
| 20,000 | 19,982 (19,976–20,022) | 228 (226–303) | 2,476 (1,935–6,882) | 0 |
| 30,000 | 29,972 (29,932–30,011) | 310 (232–325) | 5,548 (3,424–9,984) | 0 (0–1,439) |
| 40,000 | 39,860 (39,081–40,010) | 321 (257–321) | 8,178 (7,479–10,338) | 5,639 (1,414–53,688) |
| 60,000 | 58,197 (56,435–59,787) | 326 (250–335) | 9,677 (5,636–12,113) | 112,605 (11,604–212,575) |
| 100,000 | 91,723 (88,038–97,932) | 290 (275–345) | 9,587 (5,876–11,153) | 492,466 (121,967–726,402) |

- **Next to the paper:** §7.2 (real AWS regions Tokyo, Mumbai, Singapore, Ireland, São
  Paulo, 16-vCPU replicas) keeps median latency near 300 ms up to ~150k cmd/s with p99
  300–1,000 ms. Here median latency stays at 197–326 ms across the sweep (lower than the
  paper's at low load because our five-region matrix has shorter RTTs to `us-east`), while
  p99 climbs past 1.5 s at 10k/s and errors appear past 30k/s as five containers, `tc netem`
  queues, five generators and the concurrent host load (12.6–13.0) contend for 16 vCPUs.
  Real-region numbers wait on #159.

### Fig. 8: emulated WAN, adversary delaying a changing minority (§7.4)

Same 5-region emulated WAN layout and parameters as Fig. 6c–d (`us-east` leading initially,
`--max-in-flight-slots 10`, `--snapshot-every 1000000000000`, WAN timers `retransmit 400 ms`
and `hedge_max 3 s`, 60 s per offered rate, 3 runs), with a dynamic adversary (`wan.sh
adversary`) that every 5 s chooses a fresh minority (2 of 5 replicas, deterministic per run
and epoch) and adds uniform `1–500 ms` extra egress delay to all peer links of those two
replicas in place via `tc qdisc change`, restoring replicas no longer targeted. The initial
leader (`us-east`) is targeted in 5–6 of the 12 epochs per run. Measured 2026-11-24 at
`a790ff2` (`~/.local/share/quepaxa-eval/paper-fig8-9/fig8-20261124-231550`, load 18.90 at
start, 9.81 at end).

| Offered (/s) | Throughput (/s) | p50 (ms) | p99 (ms) | Errors + unanswered |
|---|---|---|---|---|
| 1,000 | 997 (988–1,000) | 437 (409–536) | 3,332 (2,007–4,873) | 0 |
| 5,000 | 4,996 (4,983–5,002) | 504 (482–699) | 6,741 (5,866–7,667) | 0 |
| 10,000 | 10,000 (9,979–10,001) | 524 (489–1,103) | 9,125 (9,045–9,702) | 0 |
| 20,000 | 19,982 (19,976–20,022) | 669 (512–835) | 10,788 (7,746–12,893) | 0 |
| 30,000 | 29,651 (29,275–29,883) | 1,249 (528–1,650) | 15,257 (9,800–15,339) | 19,237 (7,683–40,893) |
| 40,000 | 37,782 (36,549–39,257) | 820 (536–1,617) | 16,479 (10,628–17,315) | 131,629 (46,584–204,359) |
| 60,000 | 50,972 (49,447–52,179) | 615 (559–1,295) | 17,118 (16,481–18,011) | 545,971 (468,096–631,806) |

- **Next to the paper:** §7.4 reports at least 75k cmd/s under 380 ms median latency on five
  dedicated `c4.4xlarge` replicas while Multi-Paxos and Raft saturate at 2.5k cmd/s. Here
  QuePaxa stays live across the whole sweep, sustaining 100% of offered load with zero errors
  up to 20k/s (p50 437–669 ms vs 197–228 ms fault-free in Fig. 6c–d) and reaching 51.0k/s at
  60k/s offered.
- **Why median latency is 437–669 ms and errors appear from 30k/s:** (1) each epoch delays
  two of five replicas' outgoing peer links by up to 500 ms, hitting the leader in 5–6 of 12
  epochs (triggering miss-based rotations to `us-west` or `eu-west`) and adding the egress
  delay directly to victim followers' forwarded writes; (2) all five containers, five
  generators and `tc netem` share one 16-vCPU VM (start load 18.90), so at 30k–60k/s victim
  backlogs hit `kv-tokio`'s byte-bounded admission control / `max_outstanding_proposals`
  (`unanswered` stayed 0; `errors` are `Busy` rejections reported by `kv-bench --protocol
  etcd`). Real-region numbers wait on #159.

### Fig. 9: emulated WAN, leader failure across hedging delays (§7.5)

Five Docker containers across the same 5-region matrix (average pairwise RTT 159 ms vs the
paper's 180 ms; initial leader `us-east`), 25,000 puts/s total (5,000/s per region),
`--max-in-flight-slots 10`, `--snapshot-every 1000000000000`, `--retransmit-ms 400`, 40 s
per run with the leader killed (`SIGKILL`) at `t = 15 s`, 3 runs per delay (`wan.sh
hedge-crash`). Fixed hedging delays `50–500 ms` pin `--hedge-min-ms D --hedge-max-ms D`
(which also disables `silent_leader_takeover` below `D`, isolating fixed hedging delay `δ`);
`adaptive` runs QuePaxa's default WAN timers (`hedge_min 20 ms`, `hedge_max 3 s` with
`silent_leader_takeover`). Measured 2026-11-24 at `a790ff2`
(`~/.local/share/quepaxa-eval/paper-fig8-9/fig9-20261124-231550`, load 7.32 at start, 3.20 at
end).

Pre-crash throughput covers `0–15 s` and `Pre p50` covers settled `5–14 s` (all 5 regions,
25k/s offered). Once `us-east` is killed, its local generator cannot write; post-crash
throughput (`15–40 s`, 20k/s offered), cluster-wide recovery gap (longest stretch in
`15–40 s` with no completion across any surviving region), worst-region longest gap and post
latency cover `15–40 s` on the four surviving regions; `Errors + unanswered`, `Hedged` and
`Egress (MiB)` are whole-run totals across the four surviving regions.

| Hedging delay (ms) | Pre-crash (/s) | Post-crash (/s) | Recovery gap (ms) | Worst-region gap (ms) | Pre p50 (ms) | Post p50 (ms) | Post p99 (ms) | Errors + unanswered | Hedged | Egress (MiB) |
|---|---|---|---|---|---|---|---|---|---|---|
| 50 | 23,856 (23,112–24,071) | 19,829 (19,801–20,114) | 658 (524–999) | 1,955 (1,468–3,011) | 892 (583–2,474) | 856 (833–865) | 4,070 (1,615–4,179) | 0 | 1,334,641 (1,241,110–1,451,133) | 2,476 (2,415–4,088) |
| 100 | 24,048 (23,810–24,100) | 19,900 (19,737–20,000) | 1,027 (993–1,037) | 5,818 (5,040–7,609) | 1,006 (539–1,498) | 872 (863–903) | 7,071 (5,077–7,331) | 0 | 1,639,220 (1,577,852–1,756,361) | 2,005 (1,956–2,046) |
| 150 | 23,502 (23,173–23,686) | 20,162 (20,048–20,203) | 979 (417–1,042) | 4,978 (1,590–6,377) | 724 (564–2,243) | 871 (824–871) | 5,376 (2,860–6,897) | 0 | 1,749,615 (1,400,177–1,771,857) | 1,965 (1,866–2,009) |
| 200 | 24,550 (24,516–24,578) | 19,566 (19,550–19,682) | 1,059 (691–1,185) | 4,767 (4,069–5,530) | 387 (237–521) | 893 (868–921) | 6,377 (6,077–7,155) | 0 | 1,292,879 (1,169,122–1,484,884) | 1,377 (1,346–1,485) |
| 300 | 24,622 (24,547–24,640) | 19,916 (19,899–19,951) | 1,104 (1,104–1,109) | 4,628 (3,627–8,694) | 237 (234–1,058) | 419 (397–649) | 8,345 (5,907–9,070) | 0 | 997,458 (658,811–1,082,394) | 942 (664–992) |
| 400 | 24,585 (24,548–24,637) | 19,905 (19,902–19,992) | 1,300 (916–1,307) | 5,281 (4,474–6,415) | 229 (228–242) | 403 (387–404) | 7,591 (6,349–8,188) | 0 | 808,680 (568,598–814,678) | 712 (605–725) |
| 500 | 24,582 (24,526–24,639) | 19,927 (16,258–19,979) | 1,839 (1,495–2,360) | 9,611 (5,167–10,180) | 227 (227–227) | 1,841 (1,399–5,787) | 12,975 (9,977–13,886) | 0 (0–29,909) | 866,307 (742,487–1,469,263) | 829 (827–2,188) |
| adaptive | 24,575 (24,543–24,631) | 19,911 (19,883–19,971) | 1,493 (1,426–2,208) | 3,514 (2,524–6,419) | 225 (224–226) | 404 (404–3,817) | 7,074 (5,206–12,209) | 0 | 207,424 (114,752–364,314) | 654 (380–1,326) |

- **Liveness across all hedging delays (next to §7.5):** §7.5 reports 25k cmd/s sustained
  across `50–500 ms` hedging delays (even at `50 ms`, ~1/3 of the 180 ms average RTT), while
  Multi-Paxos and Raft fail to stabilize below 200 ms and require timeouts ≥ 1.8× RTT
  (~330 ms). Here QuePaxa stays live and sustains the full rate at every delay from `50 ms`
  (~1/3 of our 159 ms average RTT) to `500 ms` and `adaptive`: `23.5k–24.6k/s` pre-crash and
  `19.6k–20.2k/s` post-crash on the four surviving regions, zero median errors/unanswered,
  and 24/24 runs rotating cleanly to `us-west`.
- **Recovery time vs hedging delay:** §7.5 reports 303–473 ms recovery time (lowest around
  200 ms). Here the cluster-wide recovery gap rises with hedging delay from `658 ms` at
  `50 ms` to `1,059 ms` at `200 ms` (`764 ms`, range `691–1,059 ms`, inside the immediate
  `15–20 s` crash window) to `1,839 ms` at `500 ms`. Gaps are ~2–3× the paper's because
  followers forward to the leader rather than submitters broadcasting to every replica,
  rotation waits for 8 missed slots (`LAG = 64`), and single-host CPU contention at 25k/s
  stretches failover drain (in one of three `500 ms` runs, 29,909 tail writes remained
  unanswered at the 5 s post-run drain cutoff, with 0 errors).
- **Bandwidth cost of aggressive hedging (Fig. 9 bottom):** setting `δ` below the ~200 ms
  commit path causes heavy redundant hedging (`1.33M–1.75M` hedged entries and
  `1,965–2,476 MiB` survivor egress at `50–150 ms`, lifting pre-crash p50 to `724–1,006 ms`).
  Once `δ ≥ 300 ms`, pre-crash p50 settles to `227–237 ms` and egress drops to
  `712–942 MiB`. `adaptive` gives the lowest pre-crash p50 (`225 ms`), lowest hedging
  (`207k`, 6–8× fewer) and lowest egress (`654 MiB`), with a `1,493 ms` recovery gap
  (`1,426 ms` in `15–20 s`). Real-region numbers wait on #159.

### Fig. 10: convergence to the best leader among heterogeneous machines

Five Docker containers on a LAN (no `netem` delay) with increasing CPU quotas — `1`, `1.5`,
`2`, `3` and `4` CPUs on disjoint cpusets `0`, `1-2`, `3-4`, `5-7`, `8-11` (`us-east`
smallest, `sa-east` strongest), generators on `12-15`, initial leader `us-east`, 40,000
puts/s for 240 s, `--placement-periods 10000,10000`, leader sampled every 1 s
(`fig10-20261122-124323`, load 12.21 at start).

| Policy | Run | Leader changes | At `sa-east` for good (s) | Final leader | p50 0–10 s (ms) | p50 160–240 s (ms) | p99 160–240 s (ms) |
|---|---|---|---|---|---|---|---|
| `--placement on` | 1 | 26 | – | `us-east` | 2.25 | 2.07 | 5.92 |
| `--placement on` | 2 | 20 | – | `us-east` | 2.06 | 2.05 | 5.01 |
| `--placement on` | 3 | 16 | – | `us-east` | 2.09 | 2.26 | 5.85 |
| `--placement off` | 1 | 20 | – | `ap-ne` | 2.08 | 2.08 | 5.40 |
| `--placement off` | 2 | 0 | – | `us-east` | 2.09 | 2.16 | 4.92 |
| `--placement off` | 3 | 6 | – | `us-east` | 2.09 | 2.23 | 5.92 |

- **Why this did not reproduce the paper's separation:** §7.6 ran 80k cmd/s across five
  distinct EC2 types (`t2.large` to `c4.4xlarge`) where the weakest leader kept Raft/Paxos
  at 5.2 ms and QuePaxa converged in 4 s to 3.8 ms. Here 40k puts/s on tmpfs does not
  saturate even the 1-CPU `us-east` leader (`placement off` run 2 held `us-east` for all
  240 s with 0 leader changes at p50 2.16 ms, p99 4.92 ms). Every member delivers ~2.1 ms
  median and ~5–6 ms p99, so placement sees no member faster by its 10% + 3σ bar.
- **Frequent rotations under noise:** both `placement on` (16–26 changes, only 2–3 of them
  `placement_moves` per member) and `placement off` (0–20 changes) rotated repeatedly under
  the 12.2–17.1 host load (see also #240 for healthy-leader rotations at low load). To test
  heterogeneous convergence cleanly on this host, the initial leader's quota must actually
  be bottlenecked by the offered rate while the stronger replicas are not.

## Not measured

- No comparison against the Go reference implementation: running the Go code was out of
  scope of the research (it was read, not built). The workload above mirrors its shape so
  a comparison can be added on the same machine.
- No deployment on real regions: the cross-region comparison above emulates the links on
  one host.

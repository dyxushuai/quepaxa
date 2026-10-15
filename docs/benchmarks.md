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

## Not measured

- No comparison against the Go reference implementation or Raft libraries yet: running
  the Go code was out of scope of the research (it was read, not built). The workload above
  mirrors its shape so a comparison can be added on the same machine.
- Leader-failure recovery time and heterogeneous WAN placements are covered functionally by
  the simulator (`leader_crash_stalls_commits_only_briefly`) and the `kv-tokio` cluster test,
  not benchmarked here.

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

| Offered rate | Achieved | Errors | p50 | p90 | p99 |
|---|---|---|---|---|---|
| 5,000/s | 4,974/s | 0 | 0.32 ms | 0.82 ms | 1.5 ms |
| 20,000/s | 20,095/s | 0 | 0.39 ms | 0.58 ms | 2.8 ms |
| 50,000/s | 49,954/s | 0 | 0.54 ms | 0.89 ms | 30.0 ms |
| 100,000/s | 99,842/s | 0 | 0.79 ms | 30.7 ms | 124 ms |
| 200,000/s | 185,812/s | 70,476 (`Busy`) | 5.8 ms | 55.7 ms | 138 ms |

Saturation sets in near 100k operations/s; beyond it `max_outstanding_proposals` (4096
per replica) pushes back with `Busy` instead of queueing without bound.

### Simulated WAN

`run-local.sh 5000 8 50` adds a 50 ms one-way `netem` delay to all loopback traffic,
including the client connections (so every client request also pays a 100 ms round trip):

| Offered rate | Achieved | p50 | p90 | p99 |
|---|---|---|---|---|
| 5,000/s | 4,926/s | 319 ms | 596 ms | 919 ms |

The median matches the path: client round trip (100 ms) + forward to the leader (50 ms) +
leader fast path (100 ms) + decision broadcast to the client's replica (50 ms), plus fsync.

## Not measured

- No comparison against the Go reference implementation or Raft libraries yet: running
  the Go code was out of scope of the research (it was read, not built). The workload above
  mirrors its shape so a comparison can be added on the same machine.
- Leader-failure recovery time and heterogeneous WAN placements are covered functionally by
  the simulator (`leader_crash_stalls_commits_only_briefly`) and the `kv-tokio` cluster test,
  not benchmarked here.

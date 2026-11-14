# kv-tokio orders on one thread and applies on another

kv-tokio, the reference runtime, runs each replica as four parts:

- **Tokio IO tasks** read peer and client sockets and await room in one bounded event queue (65,536 events). They write peer frames from per-peer queues and client replies in request order.
- **The core thread** owns the `Replica` and is the only thread that calls it, so the core's determinism contract ([ADR 0001](0001-sans-io-core.md)) holds as it does in the simulator. It takes up to 513 events per turn, runs due timers first, and executes actions. It does not apply anything. Persists go to the writer thread. Commits, proposal registrations, released reads and snapshot requests go to the apply thread, as one batch per turn through a bounded FIFO queue of 64 batches.
- **The writer thread** makes the persists handed over during one fsync durable with the next one (group commit) and reports back. Gated messages leave only after that report.
- **The apply thread** owns the KV state and the clients waiting on proposals. It applies commits in slot order and answers each client from the result of its own slot. A snapshot is taken here and encoded and saved on a short-lived thread.

Every job the apply thread sees comes from one FIFO in the order the core emitted it. Three properties follow from that order:

- **Writes.** A write is answered after its slot, and every slot before it, is applied.
- **Linearizable reads.** The core hands a read over only when its barrier is ready (`Action::ReadReady`), behind every commit emitted before that. So the read sees every write decided before the barrier. Local (`STALE`) reads are served from the applied prefix at their place in the queue.
- **Snapshots.** The core asks for a snapshot at the last slot it handed over, together with `core_state`, which covers exactly the commits emitted so far. Compaction waits for `Ev::SnapshotSaved`, so the log is cut only through a state that was both applied and saved. An installed snapshot replaces the state at its place in the queue, and later commits apply on top of it.

A full apply queue blocks the core. That is the only time the core waits for the apply thread. The wait then backs up the event queue and, through it, peers and clients, as the writer's `MAX_UNWRITTEN` does for the disk.

## Why not a single owner

With one thread per replica running the core and all IO (`current_thread` Tokio, due timers first), #213 stage 2 needed about 30% less CPU per operation and 5-7x fewer context switches. But a thread can use only one core. In the #155 layout, with two CPUs per replica, saturation throughput fell 15% (800k → 681k/s offered 800k/s) and p50 at 10k/s doubled (202 → 434 µs). A single owner only pays with one core per replica. It was not landed.

## Why not an async core

An async core would tie the library to a runtime and lose deterministic simulation and model checking on the real code ([ADR 0001](0001-sans-io-core.md)). The threads here are the host's choice. The core stays a plain state machine on its own thread.

## Measurements

All in the #155 layout: three replicas on tmpfs, two CPUs each, and the `run-local.sh`
workload. Medians of interleaved runs; details in [benchmarks](../benchmarks.md).

- **Stage 1 (#220).** Event-queue backpressure became awaitable (a bounded Tokio channel, the core sleeping on a bell) instead of a 200 µs sleep loop. At 800k/s offered, throughput rose 1.6% and p99 fell 8%. It held elsewhere.
- **Stage 3 (this structure)**, against stage 1. Three runs per point. The 10k/s row is six extra pairs run at a 1-minute load of at most 2.6.

| Offered | Throughput | p50 | p99 | Core thread CPU |
|---|---|---|---|---|
| 10k/s | same | 192.5 → 211 µs | 852 → 847 µs | 8.7 → 8.5% |
| 200k/s | same | 590 → 610 µs | 17.7 → 10.7 ms | 40 → 20% |
| 250k/s | same | 680 → 697 µs | 35.9 → 17.6 ms | 47 → 23% |
| 800k/s | 800k → 800k | 610 → 164 ms | 4.42 → 1.61 s | 87 → 28% |

The cost is low-load latency. p50 at 10k/s rises by about 18 µs, beyond run-to-run noise:
six quiet pairs, ranges 189-208 vs 207-223 µs. Each commit now wakes one more thread
(process context switches 35k → 41k/s). We accept it. With a disk, an fsync of milliseconds
dominates the commit path. The tail and saturation gains are large. Applying inline when
the queue is empty would win the 18 µs back, but the KV state would then have two
owners, so we do not do it.

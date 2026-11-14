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

MEASUREMENTS

# How do existing Rust consensus crates shape a sans-IO API

Parent: [map](../map.md)
Type: research
Status: resolved
Blocked by: 

## Question

Study API shapes of raft-rs (Ready/RawNode), openraft, omnipaxos, and any other sans-IO protocol crates (quinn-proto, rustls) for: input events vs output actions, time and randomness injection, storage traits and persistence ordering guarantees, batching, error handling, no_std support. Extract lessons and pitfalls relevant to QuePaxa.

## Answer

Full survey: `docs/research/sans-io-api-prior-art.md` on branch `research/sans-io-api-prior-art` (commit 0fb71f6). Covered raft-rs 0.7.0, openraft 0.10.0-alpha.37, omnipaxos 0.2.3, quinn-proto, rustls, str0m 0.24.1, s2n-quic-core 0.90.0, etcd-io/raft.

- Recommended shape: pull loop `handle(now, Input)` then drain `poll_actions()`; must drain after every mutation (str0m rule). No callbacks, no core-held `Storage` trait.
- Persist-before-send must be structural: `Action::Persist { id, ops, then }` carries the messages gated on durability (etcd `MsgStorageAppend.Responses` pattern).
- Cautionary bugs: openraft#2118 (fixed 2026-09; an unconditional completion was treated as an fsync and acked a write lost later) → monotonic completion ids tied to one concrete write, single ordered stream, local vote waits for real durability. raft-rs#426 (durable vs in-memory progress divergence). omnipaxos panics on storage errors and its RocksDB store doesn't `sync`.
- Time: core-owned `Timestamp(u64)` (no `Instant` in no_std). Randomness: `seed: [u8;32]` at construction, fixed-width sampling for 32/64-bit determinism.
- Buffers: `Bytes`-like payload handles; output into caller `&mut Vec`; FFI encode with `InsufficientSize { required }`.
- Errors: `Err` only for caller contract violations; storage I/O errors stay with the caller, core gets `StorageFailed` and fences itself.
- Testing: seeded single-thread simulator with network + disk model (drop unsynced writes on crash), invariants every step, post-heal liveness phase.

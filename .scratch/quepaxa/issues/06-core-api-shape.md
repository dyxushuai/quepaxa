# What is the core sans-IO API

Parent: [map](../map.md)
Type: grilling
Status: resolved
Blocked by: 01, 02, 04

## Question

Decide the core crate's public surface: input events (messages, ticks/time, proposals, storage completions), output actions (send, persist, decided entries, timers), how time and randomness are injected, the storage contract and persistence ordering, read_barrier semantics, configuration knobs, error model, and no_std+alloc constraints.

## Answer

Decided 2026-10-13 (user accepted all recommendations). Detail on mechanisms deferred to: durable field set → durability ticket; hedging/leader policy → hedging ticket; encoding → wire-format ticket.

1. **Unit**: one public `Replica` (proposer + recorder). `Proposer`/`Recorder` are crate-internal, unit-tested against the Go impl's transition tables. Recorder-only (witness) nodes deferred.
2. **Driving**: pull loop — `handle(now, Input)`, then drain `poll_actions(&mut Vec<Action>)` before the next `handle`; timers via `next_deadline()` + `handle_timeout(now)`. No callbacks, no trait objects held by the core.
3. **Persist-before-send is structural** ([ADR 0002](../../../docs/adr/0002-structural-persist-before-send.md)): `Action::Persist { id: PersistId, ops, then: Vec<Outbound> }`; `then` may only be sent after `ops` are durable; `PersistId` strictly increasing and tied to one concrete write; caller reports cumulative `Input::Persisted { id }` or `StorageFailed { id }` (replica fences itself).
4. **Consensus value**: v1 carries the full batch inline in `record` messages; message layout reserves a value-kind tag so `ValueId` indirection can come later (fog).
5. **Payload**: `bytes::Bytes`; no generic value type.
6. **Batching/pipelining in the core**: `Propose { payload }` queues locally; core cuts batches and assigns slots per `Config` (`max_batch_bytes`, `max_batch_delay`, `max_in_flight_slots`).
7. **Proposal identity**: `Propose` returns a local monotonic `ProposalId(u64)`; guarantee: a local `ProposalId` is decided in at most one slot. Cross-replica/client dedup is the application's job.
8. **Outputs**: `Action::Commit { slot, entries }` in strict slot order, entries tagged with origin `(NodeId, ProposalId)`; `Input::ReadBarrier { token }` → `Action::ReadReady { token, slot }`; `Action::Dropped { id, reason }` for rejected/abandoned proposals.
9. **Time/randomness**: caller-supplied monotonic `Timestamp(u64)` nanos; `Config.seed: [u8; 32]` from a CSPRNG, ChaCha inside the core; documented contract: a fresh seed on every start incl. restart (liveness needs priorities unpredictable to the network). Fixed seeds only in simulation.
10. **Errors**: `Err` only for caller contract violations (undrained actions, out-of-order `PersistId`, non-member sender); malformed/stale network messages dropped and counted in `metrics()`; core never panics, invariants via `debug_assert` + simulator.

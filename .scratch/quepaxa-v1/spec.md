# quepaxa v1 — spec (as built)

Status: implemented (2026-10-13). This file states *what* v1 is; §12 lists every place it
deliberately differs from the spec written before implementation, with the PR that changed
it. Rationale for the original decisions: the wayfinder map `.scratch/quepaxa/map.md` and its
tickets. Vocabulary: `GLOSSARY.md`. Lasting trade-offs: `docs/adr/`.

## 1. Product

An open-source Rust implementation of the QuePaxa crash-fault-tolerant consensus protocol
(Tennage et al., SOSP'23; https://bford.info/pub/os/quepaxa/quepaxa.pdf) exposed as a
**replicated log of opaque batches**, usable from Rust and, through a stable C ABI, from any
language.

- Fault model: crash-recovery, `n = 2f + 1` replicas, no Byzantine faults.
- Membership: static in v1. `cluster_id` and `config_epoch` are in every message for
  phase-2 reconfiguration.
- Non-goals for v1: reconfiguration / dynamic join / witnesses / learners, ValueId
  indirection, the paper's latency-tuned leader order (#28), KV/lease/transaction layers,
  a published runtime crate, wasm or mobile language packages.

## 2. Workspace

| Crate | Kind | Notes |
|---|---|---|
| `quepaxa` | lib, `#![no_std]` + `alloc` | sans-IO core ([ADR 0001](../../docs/adr/0001-sans-io-core.md)). Only dependency: `bytes`. ChaCha20 (RFC 8439) and CRC-32C are in-crate. Feature `model` is test support (cloneable, fingerprintable replica), not stable API. |
| `quepaxa-ffi` | `cdylib` + `staticlib` + `rlib` | hand-written `extern "C"`; cbindgen 0.29.4 header committed at `crates/quepaxa-ffi/include/quepaxa.h`, exported symbols at `symbols.txt`. |
| `quepaxa-sim` | lib, `publish = false` | deterministic simulator, delay-bounded model checker, KV linearizability oracle. |
| `examples/kv-tokio` | binaries, `publish = false` | reference runtime: TCP, fsync'd storage, snapshots, `kv-bench` load generator. |

Edition 2024, MSRV 1.89, license `MIT OR Apache-2.0`.

## 3. Protocol (per slot)

Paper Alg. 3 (recorder) and Alg. 4 (proposer), with:

- **Step clock** `step = 4·round + phase` from 4, `u32`. Steps above `u32::MAX − 16` are
  rejected on the wire (overflow guard; correct replicas never approach it).
- **Proposal** `(priority: u64, value_key, value)`, ordered by `(priority, value_key)`;
  `value_key = (origin, incarnation, batch_seq)` is unique per value, so distinct values
  never tie. `H = u64::MAX` only for the slot's leader at step 4; every other phase-0
  priority is drawn per recorder (A1) uniformly from `[1, H−1]` with the seeded ChaCha20.
- **Recorder register** `(S, Fc, Ac, Ap)` per slot, as Alg. 3.
- **Proposer** as Alg. 4. Catch-up adopts the first reply whose step is ahead (any such
  reply is valid by Lemma C.3). Replies carry `req_step`; a reply to an older request never
  counts toward the current quorum; replies with `step < req_step` are rejected.
- **Decisions** are broadcast (`Decided`); a recorder answers requests for a decided slot
  with `Decided`. Every message carries the sender's commit index; a replica more than one
  slot behind fetches pages of `Entries` with `CatchUp` (timer-retried, page after page,
  resumed after snapshot installation).

### Leader schedule, forwarding and hedging

- The schedule is derived from the decided log only: the schedule for slot `k` is the
  latest one decided at a slot `≤ k − 64` (`max_in_flight_slots ≤ 64` guarantees every
  replica knows it for every slot it drives). Bootstrap order: members sorted by id. When
  8 consecutive decided slots did not originate at their leader, the leader moves to the
  back (effective 64 slots later; no further rotation is counted until then).
- Non-leaders `Forward` entries to the current leader and propose them themselves if they
  are not committed within the hedge delay δ = clamp(2 × EWMA(propose→commit),
  `hedge_min`, `hedge_max`) (`hedge_max/4` before the first sample).
- A slot someone else started is driven by this replica after `(position + 1)·δ` without
  progress (the local recorder seeing its step advance restarts the wait).

### Batching, pipelining, at-most-once

- `propose(payload)` / `read_barrier()` return a `ProposalId` (monotonic per incarnation).
  Batches are cut, up to `max_batch_bytes`, whenever the window (`max_in_flight_slots`)
  has room; there is no batching delay. `max_outstanding_proposals` bounds pending local
  proposals (`Busy`).
- Commit-time dedup: an entry whose `(origin, incarnation, id)` already committed is skipped
  (per-origin watermark + set, 4 incarnations kept), so forwarding plus hedging applies each
  entry once. A local id appears in at most one `Commit`.

## 4. Durability and recovery

[ADR 0002](../../docs/adr/0002-structural-persist-before-send.md): persist-before-send is
enforced by the action type.

Durable records (core-encoded, `"QR"` v1, CRC-32C): `Incarnation(n)` (first, every start),
`RecorderState { slot, register }`, `LeaderReservation { through }`, `Decided { slot,
proposal }`, `Compacted { through, core_state }`.

1. A reply is released only after every write that produced the state it reports is
   durable: it rides in the `then` of the persist carrying the write, or is parked behind the
   last in-flight persist (no empty persists are emitted).
2. No message leaves a replica before its new incarnation is durable (on restart,
   `Commit`s for already-durable decided slots are re-emitted locally first); the leader's
   own `H` leaves only after a durable reservation; `Commit` waits for the durable decision.
3. After restart a replica never uses `H` for a slot ≤ its recovered reservation.
4. `Replica::new` takes every durable record in write order; a corrupt record is
   `CorruptRecord`, never skipped. Slots with recovered recorder state are driven.
5. `StorageFailed` fences the replica. Fenced proposals are reported `Dropped(Fenced)` with
   an **unknown** outcome (they may still commit, as after a crash).
6. The seed must be fresh CSPRNG output on every start (documented contract).

## 5. Compaction and snapshots

- A recorder drops a slot's register once `Decided` for it is durable.
- `core_state()` encodes (v2) the dedup state and the leader schedule **as of the emitted
  prefix** (`emitted_index()`); the application stores it with its own snapshot, then calls
  `compact(through, core_state)`. The replica then emits a **checkpoint**:
  `Persist { replace: true }`, whose records (incarnation, reservation, `Compacted`, live
  registers, decided slots above the floor) atomically replace every earlier record.
- Requests or catch-up below the floor get `Compacted { through }`; the requester emits
  `SnapshotNeeded { peer, through }`. `install_snapshot(through, core_state)` returns
  `Ok(true)` when adopted, `Ok(false)` when the replica already emitted past it (do not
  rewind the application). Local proposals the snapshot contains are reported
  `Committed`/`ReadReady` at `through`. After a restart, an application whose snapshot is
  newer than the replica's last durable checkpoint calls `install_snapshot` with it.

## 6. Rust API (`quepaxa`)

`Replica::{new, propose, read_barrier, handle, handle_timeout, next_deadline, poll_actions,
core_state, compact, install_snapshot, commit_index, emitted_index, metrics, id}`;
`Input::{Message, Persisted, StorageFailed}`; `Action::{Persist { id, records, then,
replace }, Send, Commit { slot, entries }, Committed { id, slot }, ReadReady { id, slot },
SnapshotNeeded { peer, through }, Dropped { id, reason }}`; `Config` (`cluster_id`, `id`,
`members`, `seed`, `max_batch_bytes`, `max_message_bytes`, `max_in_flight_slots`,
`max_outstanding_proposals`, `retransmit_interval`, `hedge_min`, `hedge_max`); `Metrics`
(counters + gauges); `Error`.

Contracts: drain `poll_actions` after every call; `Err` only for contract violations;
malformed network input is dropped and counted. `Config` validation: unique members
including `id`; `max_message_bytes ≥ 2·max_batch_bytes + 1 KiB`; `max_in_flight_slots ∈
[1, 64]`; `0 ≤ hedge_min ≤ hedge_max > 0`. `cluster_id`, `members` and `max_batch_bytes`
must be identical on every member.

## 7. Wire format

Hand-rolled, little-endian, zero-copy payloads, every length bounded before use; proposal
values are bounded by `max_batch_bytes` and messages by `max_message_bytes`.

Envelope: `"QP"`, `wire_version` 1, `kind`, `cluster_id[16]`, `config_epoch u64`,
`from u32`, `to u32`, `commit_index u64`. Kinds: 1 `RecordRequest {slot, step, proposal}`,
2 `RecordReply {slot, req_step, step, first, prev_agg}` (a value equal to the request's is
elided), 3 `Decided {slot, proposal}`, 4 `CatchUp {from}`, 5 `Entries {first, proposals}`
(contiguous), 6 `Forward {batch}`, 7 `Compacted {through}`. No wire checksum; durable
records carry CRC-32C. In 0.x the wire version must match exactly; from 1.0 a release
interoperates with the previous minor.

## 8. C ABI (`quepaxa-ffi`, ABI 0.3)

`qpx_abi_version`, `qpx_status_str`, `qpx_last_error` (thread-local detail of the latest
failing call), `qpx_config_init(cfg, size)`, `qpx_replica_new/free`, `qpx_propose`,
`qpx_read_barrier`, `qpx_handle_message`, `qpx_persisted`, `qpx_storage_failed`,
`qpx_handle_timeout`, `qpx_next_deadline`, `qpx_commit_index`, `qpx_core_state` (two-call
buffer), `qpx_compact`, `qpx_install_snapshot` (`QPX_IGNORED` when stale), `qpx_get_metrics`,
`qpx_poll` + `qpx_action_record/then/entry` (borrowed until the next call on the handle).

`int32` statuses (0 ok, positive informational, negative error); every export validates
its arguments and wraps `catch_unwind` (panic → `QPX_ERR_PANIC`, handle poisoned); mutating
calls require `qpx_poll` to have returned `QPX_EMPTY`; `qpx_config` and `qpx_metrics` are
read and written only up to their `struct_size`. Handles are `Send`, not `Sync`. While the
ABI major version is 0, any minor bump may be incompatible.

## 9. Assurance

| Layer | Tool | What it checks |
|---|---|---|
| Unit / TDD | nextest | worked examples of Alg. 3/4, codec golden bytes, API contracts |
| Properties | proptest | recorder invariants R1–R6, ordering, codec round-trips, adversarial member messages |
| Single-slot simulation | `slot_sim` (core, test-only) | G1–G5 after every event under loss, duplication, reordering, partitions |
| Replica simulation | `quepaxa-sim` | agreement, validity, at-most-once, ordered commits, KV linearizability, convergence; G1/G3/G4/G5 on every replica's recorder state after every event; crash-restart losing unsynced or torn writes; snapshots |
| Model checking | delay-bounded explorer over the real `Replica` | every schedule within k deviations (reorder, loss, duplication, early timer, partitioning a replica) plus a separate crash-restart budget (losing any suffix of the write in progress; the client then retries): G1/G3/G4/G5, agreement, validity, at-most-once and `Committed` identity in every state; liveness (settling after the partition heals) at the depth bound |
| Fuzzing | bolero (libFuzzer + ASan) | wire, batch, durable record and core-state decoding |
| Unsafe code | Miri; ASan/UBSan on the C smoke test | FFI |
| Mutation | cargo-mutants (weekly) | test strength |

Per PR: fmt, clippy `-D warnings`, nextest on Tier 1, doctests, MSRV and `no_std` builds,
cargo-deny, FFI header/symbol diff, Miri, C smoke tests, model checking at 2 deviations,
semver checks, model checking (release build) of three scenarios exhausted per PR.
Nightly: 20 min fuzzing per target, 100k simulator seeds per scenario, model checking with
one more deviation per scenario, deep proptests, Criterion against the
previous night (fails above 10%), coverage. Weekly: sharded mutants. Failures open issues.

## 10. Platforms

Tier 1 (native, full tests): Linux x86_64/aarch64, Windows x86_64/aarch64, macOS
aarch64/x86_64. Tier 2: Android (cargo-ndk; C ABI tests and C smoke test on an emulator),
iOS (C ABI tests and C smoke test on a simulator), `wasm32-unknown-unknown` and
`thumbv7em-none-eabihf` (core builds).

## 11. Release

First real release `0.2.0` (crate owned by `dyxushuai`; the unrelated `0.1.0` placeholder is
yanked afterwards). Benchmarks and their method: `docs/benchmarks.md`.

## 12. Amendments made during implementation

| Spec before implementation | As built | Why | PR |
|---|---|---|---|
| `RecordReply {slot, S, Fc, Ap}` | adds `req_step` | a reply to an older request must not count toward the current quorum | #23, #24 |
| `max_batch_delay` knob | none: cut when the window has room | the window already batches under load; no idle delay | #27 |
| `Persist { id, records, then }` | adds `replace` (checkpoints); sends waiting only on in-flight writes are parked, no empty persists | log compaction; an empty persist per reply stormed the disk | #26, #31 |
| `stall_timeout` | adaptive δ (`hedge_min`/`hedge_max`), `(position+1)·δ` per stalled slot | spec'd hedging, one mechanism | #29 |
| latency-tuned leader order (explore/exploit) | deterministic rotation after 8 slots not led by the leader, 64-slot lag | needs an agreement step; paper leaves it unspecified (A3–A5); tracked in #28 | #29 |
| `read_barrier(token)` / `ReadReady {token}` | returns a `ProposalId`; `ReadReady { id, slot }` | one id space for proposals and barriers | #30 |
| `Committed { id, slot: Option }` | `slot` always present (snapshot ⇒ `through`) | simpler; snapshot resolution still reports where it was covered | #31 |
| `install_snapshot -> Result<()>` | `-> Result<bool>`, `emitted_index()` | a stale snapshot must not rewind the application | #36 |
| core state v1 (decided-prefix schedule) | v2: schedule as of the emitted prefix | v1 double-counted slots on restore | #36 |
| `Dropped` "never commits" | fenced proposals have an unknown outcome | they may already be on a quorum | #36 |
| (unspecified) | step/slot overflow bounds, value-size bound, config invariants | review of untrusted inputs | #35 |
| stateright model checking | delay-bounded explorer with liveness at the depth bound | plain interleaving search reached no seeded bug; stateright would need the same Clone/Hash surface plus 56 crates | #32, #41 |
| stateright `LinearizabilityTester` | hand-written oracle for unique-write registers | exact for this model, no dependency | #30 |
| Kani bounded proofs (optional) | not adopted | covered by fuzzing + proptests for the targeted pure functions | #41 |
| `cargo-mutants --in-diff` per PR | weekly sharded run | keeps per-PR CI fast and stable | #41 |
| C ABI `qpx_metrics`, `qpx_last_error(h, …)`, `qpx_config_init(cfg)` | `qpx_get_metrics` (C name clash), thread-local `qpx_last_error`, `qpx_config_init(cfg, size)`, strict draining, `QPX_IGNORED` | C naming; errors before a handle exists; layout growth | #33, #38 |
| Tier 2 smoke tests | C ABI tests and the C smoke test run on Android emulator / iOS simulator | as specified, plus the Rust harness | #34, #39 |
| `examples/kv-tokio` | adds snapshot transfer, group commit, bounded queues; trusted networks only | reference runtime scope | #40 |

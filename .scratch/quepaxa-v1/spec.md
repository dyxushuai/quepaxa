# quepaxa v1 — implementation spec

Status: ready for implementation (2026-10-13). Source of truth for *why*: the wayfinder map `.scratch/quepaxa/map.md` and its resolved tickets; this file states *what* to build. Vocabulary: `GLOSSARY.md`. Decisions with lasting trade-offs: `docs/adr/`.

## 1. Product

An open-source Rust implementation of the QuePaxa crash-fault-tolerant consensus protocol (Tennage et al., SOSP'23; https://bford.info/pub/os/quepaxa/quepaxa.pdf) exposed as a **replicated log of opaque batches**, usable from Rust and, through a stable C ABI, from any language.

- Fault model: crash-recovery, `n = 2f + 1` replicas, no Byzantine faults.
- Membership: static in v1. `cluster_id` and `config_epoch` are reserved in every message for phase-2 reconfiguration.
- Non-goals for v1: reconfiguration/dynamic join/witnesses/learners, ValueId indirection, KV/lease/transaction layers, a published runtime crate, wasm or mobile language packages.

## 2. Workspace

| Crate | Kind | Notes |
|---|---|---|
| `quepaxa` | lib, `#![no_std]` + `alloc` | sans-IO core ([ADR 0001](../../docs/adr/0001-sans-io-core.md)). Deps: `bytes`, a ChaCha RNG (`rand_chacha` or `chacha20`, pick the smaller audited one). |
| `quepaxa-ffi` | `cdylib` + `staticlib` + `rlib` | hand-written `extern "C"`, cbindgen header committed at `quepaxa-ffi/include/quepaxa.h`, packaged with cargo-c. |
| `quepaxa-sim` | lib, `publish = false` | deterministic simulator + invariant oracles + toy KV. |
| `examples/kv-tokio` | example | TCP transport + fsync'd file storage + toy KV; benchmark harness. |

Edition 2024, MSRV 1.89, license `MIT OR Apache-2.0`.

## 3. Protocol (per slot)

Follows the paper's Alg. 3 (recorder) and Alg. 4 (proposer). The map's paper-protocol ticket holds the citations. Implementation decisions on top of the paper:

- **Step clock**: `step = 4·round + phase`, starting at 4. Step is `u32`; overflow is an error (it never happens in practice).
- **Proposal** = `(priority: u64, value_key, value)`, ordered by `(priority, value_key)`. `value_key = (origin: NodeId, incarnation: u64, batch_seq: u64)` is unique per distinct value, so distinct values never tie. Nil = absent. `H = u64::MAX` is reserved for the leader at step 4. Every other phase-0 priority is drawn **per recorder** (A1: `p_j`) uniformly from `[1, H−1]` using the seeded ChaCha.
- **Recorder register** `(S, Fc, Ac, Ap)` per slot, exactly as Alg. 3 with `max` over the order above.
- **Proposer**: as Alg. 4. Catch-up adopts the reply with the **maximum** `s′` (A12). It decides at step 4 if every quorum reply's `Fc` has priority `H`, or at phase 2 if `p = max(a′)`. A `Decided` reply ends the slot with that value.
- **Decision dissemination**: the deciding proposer broadcasts `Decided { slot, value }`. Every message header carries the sender's commit index. A replica that is behind sends `CatchUp { from_slot }` and gets back `Entries` or `Compacted`.

### Leader schedule and hedging

- The schedule is a core-internal log entry, `ScheduleChange`. The schedule for slot k is the latest one decided at a slot ≤ k − W, where W = `max_in_flight_slots`. No slot k starts before slot k − W is decided. The bootstrap schedule is the members sorted by `NodeId`.
- Default tuner: epochs of `epoch_slots` (default 256). Explore: 2 epochs per member, round-robin. Exploit: sort members **ascending** by mean epoch commit latency. Only the leader proposes `ScheduleChange`. A static schedule is available through config.
- Hedging: the proposer at schedule position k waits `(k−1)·δ` and restarts that wait whenever its colocated recorder sees the slot's step advance, or the slot is decided. The default is adaptive: δ = clamp(2 × EWMA of fast-path commit latency, `hedge_min` = 1 ms, `hedge_max` = 5 s). A static δ is available through config.
- Non-leaders `Forward` their local proposals to the leader. If a proposal is not committed within the forwarding replica's hedge delay, that replica proposes it itself.

### Batching, pipelining, at-most-once

- `propose(payload)` returns `ProposalId(u64)`, monotonic per incarnation. The core cuts batches by `max_batch_bytes` / `max_batch_delay`, keeps ≤ `max_in_flight_slots` slots in flight and ≤ `max_outstanding_proposals` undecided proposals.
- A batch whose slot decided another value is re-proposed in a later slot (same `value_key`). An origin has each batch in at most one slot at a time.
- **Commit-time dedup**: a committed entry whose `(origin, incarnation, proposal_id)` has already committed is skipped. The per-origin state is a watermark plus a bitmap of width `max_outstanding_proposals`, and it is part of `core_state`. Guarantee: a local `ProposalId` appears in at most one `Commit` entry. Cross-client dedup belongs to the application.

## 4. Durability and recovery

[ADR 0002](../../docs/adr/0002-structural-persist-before-send.md): persist-before-send is enforced by the action type.

Durable record kinds (`WriteOp`, core-encoded, CRC32C, versioned):

| Record | Written when |
|---|---|
| `Incarnation(n)` | first action of every start, before anything else |
| `RecorderState { slot, S, Fc, Ac, Ap }` | a `record` changed `(S, Fc, Ac)` |
| `LeaderReservation { through_slot }` | before using `H` for a slot beyond the current reservation; reserved W slots ahead |
| `Decided { slot, batch }` | a slot is learned decided |
| `Compacted { through_slot }` | `compact` or `install_snapshot` |

Rules:
1. A reply is released only after every write that produced the state it reports is durable. Replies ride in the `then` of the latest pending `Persist`; if nothing is pending they are sent immediately.
2. `Commit` for slot k is emitted only after `Decided { k }` is reported `Persisted`.
3. After restart, the replica never uses `H` for a slot ≤ the recovered reservation.
4. `Replica::new` receives every durable record in write order and rebuilds all state. A CRC or decode failure is an error, never skipped.
5. `StorageFailed` fences the replica: it answers nothing and proposes nothing until restart.
6. The seed must be fresh from a CSPRNG on every start. This is a documented contract, required for liveness.

## 5. Compaction and snapshots

- A recorder drops a slot's register once `Decided` for that slot is durable. Any later `record` for that slot gets `Decided` back.
- The app persists its snapshot together with `core_state()` (taken as of the last emitted `Commit`), then calls `compact(through_slot)`. Entries ≤ through are dropped after `Compacted` is persisted.
- A request for a slot that has been compacted gets `Compacted { through }`. The requester then emits `SnapshotNeeded { peer, through }`. The app transfers the snapshot and calls `install_snapshot(through, core_state)`.
- Local proposals that fall inside an installed range are resolved from the installed dedup state: `Committed { id, slot: None }`, or re-queued. Pending read barriers complete at `max(barrier slot, through)`.

## 6. Rust API (`quepaxa`)

```rust
pub struct Timestamp(pub u64);            // monotonic nanos, caller epoch
pub struct NodeId(pub u32);
pub struct ProposalId(pub u64);
pub struct PersistId(pub u64);            // strictly increasing per incarnation
pub struct Slot(pub u64);

pub struct Config {
    pub cluster_id: [u8; 16], pub id: NodeId, pub members: Vec<NodeId>,
    pub seed: [u8; 32],                    // fresh CSPRNG seed every start
    pub max_batch_bytes: u32, pub max_batch_delay: u64, pub max_in_flight_slots: u32,
    pub max_outstanding_proposals: u32, pub max_message_bytes: u32,
    pub epoch_slots: u32, pub leader: LeaderConfig, pub hedge: HedgeConfig,
}

impl Replica {
    pub fn new(cfg: Config, durable: impl IntoIterator<Item = Bytes>, now: Timestamp) -> Result<Self, Error>;
    pub fn propose(&mut self, now: Timestamp, payload: Bytes) -> Result<ProposalId, Error>; // Busy => backpressure
    pub fn handle(&mut self, now: Timestamp, input: Input) -> Result<(), Error>;
    pub fn handle_timeout(&mut self, now: Timestamp);
    pub fn next_deadline(&self) -> Option<Timestamp>;
    pub fn poll_actions(&mut self, out: &mut Vec<Action>);   // must drain before next mutation
    pub fn read_barrier(&mut self, now: Timestamp, token: u64) -> Result<(), Error>;
    pub fn core_state(&self) -> Bytes;
    pub fn compact(&mut self, through: Slot) -> Result<(), Error>;
    pub fn install_snapshot(&mut self, through: Slot, core_state: Bytes) -> Result<(), Error>;
    pub fn metrics(&self) -> &Metrics;
}

#[non_exhaustive] pub enum Input {
    Message { bytes: Bytes },               // decoded + validated by the core
    Persisted { id: PersistId },            // cumulative
    StorageFailed { id: PersistId },
}

#[non_exhaustive] pub enum Action {
    Persist { id: PersistId, records: Vec<Bytes>, then: Vec<Outbound> },
    Send(Outbound),                         // not gated on durability
    Commit { slot: Slot, entries: Vec<Entry> },   // strict slot order
    Committed { id: ProposalId, slot: Option<Slot> },
    Dropped { id: ProposalId, reason: DropReason },
    ReadReady { token: u64, slot: Slot },
    SnapshotNeeded { peer: NodeId, through: Slot },
}
pub struct Outbound { pub to: NodeId, pub bytes: Bytes }
pub struct Entry { pub origin: NodeId, pub incarnation: u64, pub id: ProposalId, pub payload: Bytes }
```

Errors: `Err` only for caller contract violations (undrained actions, `PersistId` out of order, bad config, corrupt recovery records). Malformed or stale network input is dropped and counted in `metrics()`. The core never panics on any input; invariants are checked with `debug_assert`.

## 7. Wire format

The core owns the codec. It is hand-rolled, little-endian, fixed width, with `u32` length prefixes. Payloads are zero-copy `Bytes`. Every length is bounded by config before any allocation. One message is one frame; framing is the transport's job.

- Envelope: `"QP"`, `wire_version u8`, `kind u8`, `cluster_id [16]`, `config_epoch u64`, `from u32`, `to u32`, `commit_index u64`.
- Kinds: `RecordRequest {slot, step, proposal}`, `RecordReply {slot, S, Fc, Ap}`, `Decided {slot, value}`, `Compacted {through}`, `CatchUp {from_slot}`, `Entries {first_slot, values}`, `Forward {proposals}`.
- Proposal: `priority u64`, `value_key (u32, u64, u64)`, `value: kind u8 (0 = inline) + bytes`. A reply omits the bytes when the `value_key` equals the request's.
- No wire checksum. Durable records carry CRC32C.
- Compatibility: in 0.x the wire version must match exactly. From 1.0, a release interoperates with N−1 minor. Durable format: new code reads all older versions; old code never reads newer. Golden files pin each version.

## 8. C ABI (`quepaxa-ffi`)

- `qpx_replica_new(const qpx_config*, const qpx_bytes* records, size_t n, qpx_replica** out)` / `qpx_replica_free`. `qpx_config` starts with `struct_size`.
- Inputs: `qpx_propose`, `qpx_handle_message`, `qpx_persisted`, `qpx_storage_failed`, `qpx_read_barrier`, `qpx_handle_timeout`, `qpx_next_deadline`, `qpx_compact`, `qpx_core_state` (two-call buffer), `qpx_install_snapshot`, `qpx_metrics`.
- `qpx_poll(h, qpx_action*)` → `QPX_OK` / `QPX_EMPTY`. The result is a tagged struct with borrowed `(ptr, len)` fields; `Persist.then` is reached through `qpx_action_then(h, i, …)`. Borrowed pointers are valid until the next call on that handle.
- `int32_t` status codes. `qpx_last_error`. Every export has `catch_unwind`; a panic returns `QPX_PANIC` and later calls return `QPX_POISONED`.
- Handles are `Send`, not `Sync`.
- `QPX_ABI_VERSION_MAJOR/MINOR` and `qpx_abi_version()`; changes within a major are additive only.
- v1 ships a C example and a smoke test. wasm has no C ABI.

## 9. Assurance

| Layer | Tool | Oracle |
|---|---|---|
| Unit / TDD | nextest; Go transition tables as vectors | Alg. 3/4 behaviour, A14 leader catch-up at step 4 |
| Properties | proptest, proptest-state-machine | R1–R6, ordering totality, codec round-trip, replay equivalence |
| DST | `quepaxa-sim` | G1–G5 every step; liveness after heal; faults: drop/dup/reorder/delay/partition, crash-restart losing unsynced writes + torn last write, `StorageFailed`, snapshot install |
| Linearizability | stateright `LinearizabilityTester` | toy KV histories with read barriers |
| Model check | stateright over real `Replica`, N=3, bounded | G1–G5 with crash-restart |
| Fuzz | bolero + arbitrary | wire decode, record decode, event sequences, sim schedules |
| Unsafe | Miri (ffi), ASan/UBSan (C smoke) | no UB |
| Mutation | cargo-mutants | tests catch changes |

Gates:
- Per PR: fmt, clippy `-D warnings`, nextest, DST 1k seeds, small stateright bound, fuzz corpus replay, mutants `--in-diff`, Miri, MSRV + `no_std` builds, cargo-deny, header/symbol diff, cargo-semver-checks.
- Nightly: fuzz 1 h per target, DST 1M seeds, larger stateright bounds, Kani, criterion.
- Weekly: full sharded mutants.
- Coverage is reported, not gated.

## 10. Platforms and release

- Tier 1 (native full tests): `ubuntu-24.04`, `ubuntu-24.04-arm`, `windows-2025`, `windows-11-arm`, `macos-15`, `macos-15-intel`.
- Tier 2: Android (cargo-ndk, NDK r28+, emulator smoke), iOS (xcframework, simulator smoke), `wasm32-unknown-unknown` and `thumbv7em-none-eabihf` (core builds only).
- First release is `quepaxa` 0.2.0. The 0.1.0 placeholder (crate now owned by `dyxushuai`) is yanked after 0.2.0 ships. Also reserve `quepaxa-ffi` then.
- Publishing uses crates.io trusted publishing from tag CI. GitHub Releases carry the header, Tier 1 libraries (Linux built against glibc 2.28 via cargo-zigbuild), Android `.so` files and an iOS xcframework. This requires a public GitHub repo, which the user creates.
- Benchmarks: criterion on `record`, the codec and the replica loop. End-to-end runs use `examples/kv-tokio` on loopback and on netem WAN, with the Go repo's 17-byte KV Poisson open-loop workload. Results are hypotheses, not production claims.

## 11. Backlog

TDD-ordered tickets live on GitHub: tracking issue [#1](https://github.com/dyxushuai/quepaxa/issues/1) with sub-issues #2–#18 under milestone `v0.2.0`, blocking edges as native issue dependencies. Each starts from a failing test.

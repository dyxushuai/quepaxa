# What must persist, and how does a replica recover from crash-restart

Parent: [map](../map.md)
Type: grilling
Status: resolved
Blocked by: 02, 04

## Question

The paper assumes crash-stop. Decide the crash-restart model: exactly which recorder/proposer state is written durably before a reply is sent, what a restarted replica may and may not do before rejoining (e.g. reusing priorities or proposer state), how decided slots are learned/broadcast and caught up, and which of the paper's ambiguities A1–A16 (see the paper protocol spec answer) the design must close and how.

Note (from the phase-one scope answer): reconfiguration is not in v1, so "amnesia ⇒ rejoin as a new member" is not available as the v1 recovery strategy.

## Answer

Resolved 2026-10-13 under the "一步到位" directive (agent recommendation; user reviews).

1. **Recorder durability**: any `record` that changes `(S, Fc, Ac)` for a slot produces a durable `RecorderState { slot, S, Fc, Ac, Ap }` write. **Every reply is released only after every write that produced the state it reports is durable** — replies ride in the `then` of the latest pending `Persist`, or go out immediately if nothing is pending. Stale/no-op calls write nothing but obey the same release rule. Group commit falls out of the drain loop (one `Persist` per drain).
2. **Proposer state is not durable for safety**, with two exceptions:
   - **Leader-H reservation**: before using priority `H` for slot k, the leader must have durably written `LeaderReservation { through_slot ≥ k }` (reserved ahead in chunks of the pipeline window). After restart it never uses `H` for any slot ≤ the recovered reservation (protects G5 "unique H").
   - **Incarnation**: on every start the replica durably bumps `Incarnation(n)` before emitting any other action. Value keys and proposal origins include it, so pre-crash proposals that still decide are never confused with new ones.
3. **Decided log**: a decided slot is written as `Decided { slot, batch }`; `Action::Commit` for it is emitted only after that write is reported `Persisted`. Recorder state for that slot becomes garbage once `Decided` is durable (see compaction ticket).
4. **Decision dissemination (closes A10)**: the deciding proposer broadcasts `Decided { slot, value }` to all; every message header piggybacks the sender's commit index; a replica behind a peer's commit index pulls with `CatchUp { from_slot }` → `Entries` or `Compacted { through }`.
5. **Recovery**: the caller feeds back every durable record (opaque bytes, in write order) to `Replica::new`; the core rebuilds recorder registers, decided log, reservation, incarnation, compaction point, and core state from the last snapshot. Records are CRC32C-checked; a corrupt record fails `new` with an error (never silently skipped).
6. **Storage failure**: `StorageFailed` → replica is Fenced (answers nothing, proposes nothing) until restarted.
7. **Ambiguity closures**: A1 → per-recorder `p_j`. A2/A13 → proposal order key = `(priority: u64, value_key)` where `value_key = (origin NodeId, incarnation, batch_seq)` is unique per distinct value, so distinct values never tie; `H = u64::MAX`, random priorities uniform in `[1, H−1]` from the seeded ChaCha. A12 → catch up to the maximum `s′` in the quorum. A14 → explicit test. A9 → commit-time dedup (hedging ticket). A3–A8 → hedging ticket. A15 → covered by stateright with crash-restart (assurance ticket).

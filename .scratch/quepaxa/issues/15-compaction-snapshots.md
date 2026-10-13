# How do compaction and snapshot catch-up work safely

Parent: [map](../map.md)
Type: grilling
Status: resolved
Blocked by: 12

## Question

Given the durability/recovery design, decide: the exact GC safety condition for dropping per-slot recorder state and log entries (what evidence that a slot's decision is known suffices), how a replica learns a peer is behind its compaction point, the `compact` / `SnapshotNeeded` / `SnapshotInstalled` API contract and its persistence ordering, what happens to in-flight proposals and read barriers across a snapshot install, and how the simulator tests snapshot catch-up under crash-restart.

## Answer

Resolved 2026-10-13 under the "一步到位" directive.

1. **Recorder GC condition**: a recorder may drop its register for slot k as soon as `Decided { slot: k }` is durable locally. Afterwards any `record` for k is answered with `Decided { k, value }`, which the proposer adopts as final (safe by Agreement).
2. **Log compaction**: the app takes `Replica::core_state()` (opaque bytes: dedup state, leader schedule/tuner state, as of the last emitted `Commit`) together with its own state at the same applied slot, persists both, then calls `compact(through_slot)`; the core emits a durable `Compacted { through_slot }` write and drops decided entries ≤ it after that write is `Persisted`.
3. **Lagging peers**: `CatchUp` or `record` for a slot ≤ compaction point is answered `Compacted { through }`; the receiver emits `Action::SnapshotNeeded { peer, through }`. The app fetches the snapshot out of band and calls `install_snapshot(through_slot, core_state)`; the core persists `Compacted`, discards its own state ≤ through, and resumes catch-up from through + 1.
4. **In-flight work across install**: local proposals whose outcome falls inside the installed range are resolved from the installed dedup state: committed → `Action::Committed { id, slot: None }`, else re-queued. Pending read barriers complete with `slot = max(barrier slot, through)`.
5. **Tests**: sim scenarios with compaction under crash-restart, a replica partitioned past the compaction point, snapshot install racing with new decisions, and corrupt/lagging snapshots rejected.

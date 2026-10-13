# Are reconfiguration and snapshots in the first release

Parent: [map](../map.md)
Type: grilling
Status: resolved
Blocked by: 01, 02

## Question

Given what the paper and Go impl cover, decide whether membership reconfiguration/bootstrapping and snapshots/log compaction are in the first release, a later phase of this spec, or out of scope.

## Answer

Decided 2026-10-13 (user accepted all recommendations).

1. **Snapshots/log compaction: in v1.** App calls `compact(through_slot, snapshot_meta)` once its state covers that slot; core reclaims entries and recorder state below it only under the GC safety condition (a slot's recorder state may be dropped only once no proposer can still need a `record` reply for it). Lagging peer beyond the compaction point → `Action::SnapshotNeeded { peer }`; app transfers opaque snapshot bytes; `Input::SnapshotInstalled { through_slot }`. Mechanics → new ticket "How do compaction and snapshot catch-up work safely".
2. **Reconfiguration: not in the first release, phase 2 of this spec.** v1 is static membership. Reserve now: `config_epoch` in every message and slot numbering scoped per epoch (paper fn. 5), so phase 2 needs no wire break.
3. **Cluster identity/bootstrap**: `Config.cluster_id: [u8; 16]` carried in every message, mismatches dropped. v1 bootstrap is static (all members start with identical config); dynamic join is phase 2.
4. **Witness (recorder-only) and learner nodes**: phase 2, with reconfiguration.

Consequence: the "amnesia ⇒ rejoin as new member" durability alternative is unavailable in v1 (it needs reconfiguration).

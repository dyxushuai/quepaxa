# How do compaction and snapshot catch-up work safely

Parent: [map](../map.md)
Type: grilling
Status: open
Blocked by: 12

## Question

Given the durability/recovery design, decide: the exact GC safety condition for dropping per-slot recorder state and log entries (what evidence that a slot's decision is known suffices), how a replica learns a peer is behind its compaction point, the `compact` / `SnapshotNeeded` / `SnapshotInstalled` API contract and its persistence ordering, what happens to in-flight proposals and read barriers across a snapshot install, and how the simulator tests snapshot catch-up under crash-restart.

# 11: Compaction and snapshot catch-up

**What to build:** Recorder registers dropped once Decided is durable; core_state/compact/SnapshotNeeded/install_snapshot; in-flight proposals and barriers resolved across install.

**Blocked by:** 08

**Status:** ready-for-agent

Spec: [spec.md](../spec.md). Start every change from a failing test.

- [ ] sim: replica partitioned past the compaction point recovers via snapshot
- [ ] compaction under crash-restart keeps G1–G5
- [ ] memory stays bounded over 10M simulated slots

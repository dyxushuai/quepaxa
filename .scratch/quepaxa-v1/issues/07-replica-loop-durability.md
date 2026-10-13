# 07: Replica loop with structural persist-before-send and crash-restart

**What to build:** Public `Replica` with handle/poll_actions/timers; recorder writes become `Persist{then}`; replies released only after the state they report is durable; Incarnation and LeaderReservation records; recovery from durable records with CRC32C.

**Blocked by:** 06

**Status:** ready-for-agent

Spec: [spec.md](../spec.md). Start every change from a failing test.

- [ ] Err on undrained actions and out-of-order PersistId
- [ ] sim disk model loses unsynced writes and tears the last write on crash; G1–G5 hold across crash-restart
- [ ] restarted leader never reuses H within its reservation
- [ ] StorageFailed fences the replica

# 04: Wire codec for envelope and record messages

**What to build:** Messages encode/decode with the envelope and RecordRequest/RecordReply, zero-copy payloads, bounded lengths; mismatched cluster/epoch/version are rejected.

**Blocked by:** 03

**Status:** ready-for-agent

Spec: [spec.md](../spec.md). Start every change from a failing test.

- [ ] round-trip proptest
- [ ] golden byte files for wire_version 1
- [ ] bolero fuzz target on decode; corpus replays in cargo test
- [ ] oversized length rejected before allocation

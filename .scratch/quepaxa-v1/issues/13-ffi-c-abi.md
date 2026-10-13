# 13: C ABI with header, smoke test and ABI gates

**What to build:** `quepaxa-ffi` exposes the spec's C API; cbindgen header committed; a C example drives a 3-replica in-process cluster to commit entries.

**Blocked by:** 08

**Status:** ready-for-agent

Spec: [spec.md](../spec.md). Start every change from a failing test.

- [ ] catch_unwind on every export; panic poisons the handle
- [ ] Miri on ffi tests; ASan/UBSan on the C smoke test
- [ ] CI diffs header and exported symbol list
- [ ] QPX_ABI_VERSION exported

# What does the C ABI expose

Parent: [map](../map.md)
Type: grilling
Status: resolved
Blocked by: 05, 06

## Question

Decide the C ABI surface over the core: handles, event/action marshalling, buffer ownership, callbacks vs polling, error codes, thread-safety, panic policy, ABI versioning, header generation, and which language bindings (if any) ship as examples.

## Answer

Resolved 2026-10-13 under the "一步到位" directive. Toolchain from the FFI research (hand-written `extern "C"`, committed cbindgen header, cargo-c).

1. **Handle**: opaque `qpx_replica*`; `qpx_replica_new(const qpx_config*, const qpx_bytes* records, size_t n_records, qpx_replica** out)` (records = durable records for recovery) and `qpx_replica_free`. `qpx_config` starts with `struct_size`.
2. **Inputs** mirror the Rust API: `qpx_handle_message(h, now, bytes)`, `qpx_propose(h, now, bytes, uint64_t* out_id)`, `qpx_persisted(h, now, id)`, `qpx_storage_failed`, `qpx_read_barrier`, `qpx_handle_timeout`, `qpx_next_deadline`, `qpx_compact`, `qpx_core_state` (two-call buffer), `qpx_install_snapshot`.
3. **Actions**: `qpx_poll(h, qpx_action* out)` returns `QPX_OK` or `QPX_EMPTY`; `qpx_action` is a tagged struct (kind, peer, ids, slot, borrowed `(ptr,len)` byte fields; `Persist` exposes `then` via `qpx_action_then(h, i, ...)`). Messages and durable records are already-encoded bytes. **All borrowed pointers stay valid until the next call on the same handle.** No callbacks.
4. **Errors**: `int32_t` status enum; `qpx_last_error(h, buf, len)` for detail. Every export wraps `catch_unwind`; a panic returns `QPX_PANIC` and poisons the handle (all later calls return `QPX_POISONED`).
5. **Threading**: a handle may move between threads but calls on one handle must be serialized by the caller; distinct handles are independent.
6. **Versioning**: `QPX_ABI_VERSION_MAJOR/MINOR` in the header and `qpx_abi_version()`; additive-only within a major.
7. **Bindings in v1**: a C example + smoke test only. wasm gets no C ABI (wasm-bindgen / wasip2 later). Kotlin/Swift/Python packages are out of v1.

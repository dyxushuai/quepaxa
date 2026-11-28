# Unsafe Code and C ABI Review (#161)

Date: 2026-11-28  
Scope: Entire workspace (`crates/quepaxa`, `crates/quepaxa-sim`, `examples/kv-tokio`, `crates/quepaxa-ffi`).

## Unsafe surface inventory

- `crates/quepaxa/src`: **0** `unsafe` blocks or functions.
- `crates/quepaxa-sim/src`: **0** `unsafe` blocks or functions.
- `examples/kv-tokio/src`: **0** `unsafe` blocks or functions.
- `crates/quepaxa-ffi/src`: All `unsafe` in the repository lives in `lib.rs`, `config.rs`, and `actions.rs` (27 exported `extern "C"` symbols listed in `symbols.txt`).

Workspace lints enforce `unsafe_op_in_unsafe_fn = "deny"` and `quepaxa-ffi` denies `clippy::unwrap_used`, `clippy::expect_used`, and `clippy::panic` outside tests.

## Findings and resolutions

| # | Severity | Location | Finding | Resolution |
|---|---|---|---|---|
| 1 | High (UB) | `lib.rs`: `bytes_from`, `ids_from`, `qpx_replica_new` | `slice::from_raw_parts` was called without verifying `len * size_of::<T>() <= isize::MAX` or pointer alignment for `u32` and `qpx_bytes`. A hostile or buggy C caller passing overflowing `len` or misaligned pointers triggered immediate Rust UB before reading memory. | Added `valid_const` / `valid_mut` (non-null + `is_aligned()`) and `max_slice_len::<T>()` guards returning `QPX_ERR_INVALID_ARGUMENT`. |
| 2 | High (UB) | `lib.rs`, `config.rs`, `actions.rs` | Typed out-pointers (`*mut u64`, `*mut u32`, `*mut usize`, `*mut qpx_action`, `*mut qpx_bytes`, `*mut qpx_entry`, `*mut *mut qpx_*`) and handle pointers (`*mut qpx_replica`, `*mut qpx_config`, `*const qpx_config`) checked only `.is_null()`, not `.is_aligned()`, before `as_mut`/`as_ref`/`write`. | Checked `valid_mut` / `valid_const` across all exported entry points. |
| 3 | High (UB) | `lib.rs`: `qpx_core_state` | `ptr::copy_nonoverlapping(state.as_ptr(), buf, state.len())` ran unconditionally; if `state` were ever empty and the caller passed `(NULL, 0)` (explicitly permitted by the safety contract), passing `NULL` to `copy_nonoverlapping` even with count 0 is UB in Rust. | Guarded copy with `if !state.is_empty()`. |
| 4 | Medium (UB on unwind) | `lib.rs`, `config.rs` | `qpx_config_new`, `qpx_config_set`, `qpx_config_set_leader_order`, `qpx_config_free`, `qpx_replica_free`, `qpx_last_error`, and the pre-`mutate` `bytes_from`/`ids_from` calls in `qpx_propose`, `qpx_propose_reconfiguration`, `qpx_handle_message`, `qpx_compact`, and `qpx_install_snapshot` executed outside `catch_unwind`. | Wrapped all config/error/free entry points in `catch_unwind` (`ffi_guard`) and routed mutating entry points' input parsing through `mutate_with` inside `with`, so no panic can unwind across `extern "C"`, `LAST_ERROR` is cleared before parsing, invalid/poisoned/undrained handles reject before copying, and any parse panic poisons the handle cleanly. Also validate `out_id` before entering `mutate_with`. |
| 5 | Medium (ABI) | `lib.rs`: `QPX_ABI_VERSION_MINOR` | `QPX_CONFIG_TUNE_IN_FLIGHT_SLOTS`, `QPX_METRIC_IN_FLIGHT_SLOTS`, and `QPX_METRIC_PIPELINE_ADJUSTMENTS` were documented as ABI `0.12`, while `QPX_ABI_VERSION_MINOR` remained `11`. | Bumped `QPX_ABI_VERSION_MINOR` to `12`, documented `0.12`, and regenerated `include/quepaxa.h`. |
| 6 | Low (C UB hygiene) | `lib.rs`: `qpx_bytes::of` | Empty slices exposed Rust's non-null dangling pointer (`0x1`) instead of `NULL`, even though the C contract documents `ptr` as null-when-empty and C99/C11 forbids invalid non-null pointers in `memcpy(dst, ptr, 0)`. | Return `qpx_bytes::EMPTY` (`ptr: null(), len: 0`) when `b.is_empty()`. |
| 7 | Low (invariant) | `lib.rs`: `qpx_replica_new` | Initialized `drained: false` unconditionally instead of `h.drained = h.queue.is_empty()` after `h.pull()`, diverging from `mutate()`. | Set `h.drained = h.queue.is_empty()` after `h.pull()`. |
| 8 | Low (C string) | `lib.rs`: `set_last_error` | Did not strip interior NUL bytes; if an error string ever contained `\0`, `qpx_last_error`'s `out_len` would disagree with `strlen(buf) + 1`. | Strip interior `\0` in `set_last_error`. |
| 9 | Low (docs) | `config.rs`: `QPX_CONFIG_INLINE_BATCH_BYTES` | Header doc still claimed `default u32::MAX off` after v0.8.0 changed the default to `1024`. | Updated doc comment and regenerated `include/quepaxa.h`. |

## Verified non-issues

- **Borrowed pointer lifetime**: `qpx_poll` and `qpx_action_*` borrow from `h.current` (`Option<Action>`). `h.current` is cleared on the next `qpx_poll` or any `mutate()` call, matching the documented "valid until the next call on that handle" rule.
- **Thread safety**: `qpx_replica` is single-owner, not `Sync`, and `LAST_ERROR` is thread-local (`try_with` guards against TLS destructor teardown). `c_abi.rs` verifies cross-thread move, concurrent independent handles, and per-thread `LAST_ERROR` isolation under Miri.
- **Static status strings**: `qpx_status_str` returns pointers to `'static` NUL-terminated byte literals for every status code including unknown values.
- **Exported symbol surface**: All 27 symbols match `crates/quepaxa-ffi/symbols.txt` with no unintended exports.

## Verification

- `cbindgen --config crates/quepaxa-ffi/cbindgen.toml --crate quepaxa-ffi --output crates/quepaxa-ffi/include/quepaxa.h` (clean match).
- `cargo +nightly miri test -p quepaxa-ffi` (all unit and C ABI integration tests, including misaligned pointers, overflowing lengths, empty `qpx_bytes`, panic poisoning, and cross-thread tests).
- `examples/smoke.c` compiled and run against `libquepaxa_ffi.a`.

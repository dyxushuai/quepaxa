//! Stable C ABI over the [`quepaxa`] consensus core, for C, C++, Go, Swift, Kotlin/JNI,
//! Python (ctypes/cffi) or any language that can call C.
//!
//! The ABI exposes the same sans-IO [`quepaxa::Replica`]: your program feeds it events and
//! carries out the actions it returns. Read [`quepaxa::guide::driving`] for the model; this
//! page covers what is specific to C.
//!
//! # Getting the library
//!
//! - **Prebuilt:** every GitHub release attaches `quepaxa-ffi-<version>-<target>` archives
//!   with `include/quepaxa.h` and the static and shared libraries for Linux (x86_64,
//!   aarch64; glibc 2.28+), macOS (x86_64, arm64), Windows (x64, arm64, MSVC), Android
//!   (`jniLibs` for four ABIs, 16 KB pages) and iOS (an XCFramework).
//! - **From source:** `cargo build --release -p quepaxa-ffi` produces
//!   `libquepaxa_ffi.a` / `.so` / `.dylib` (or `quepaxa_ffi.lib` / `.dll`) in
//!   `target/release`; the header is `crates/quepaxa-ffi/include/quepaxa.h`.
//! - **Linking statically** on Linux also needs `-lpthread -ldl -lm`.
//!
//! # The loop in C
//!
//! ```c
//! #include "quepaxa.h"
//!
//! uint32_t members[3] = {0, 1, 2};
//! uint8_t seed[32];
//! fill_with_csprng(seed, sizeof seed);         /* fresh on every start */
//! qpx_config *cfg;
//! check(qpx_config_new((const uint8_t *)"my-cluster-0001!", 0, members, 3, seed, &cfg));
//! check(qpx_config_set(cfg, QPX_CONFIG_MAX_IN_FLIGHT_SLOTS, 32)); /* optional settings */
//!
//! qpx_replica *r;
//! check(qpx_replica_new(cfg, records, n_records, now_ns(), &r)); /* durable records, in order */
//! qpx_config_free(cfg);                        /* the replica keeps its own copy */
//!
//! for (;;) {                                   /* after every call: drain */
//!   qpx_action a;
//!   int32_t rc = qpx_poll(r, &a);
//!   if (rc == QPX_EMPTY) break;
//!   switch (a.kind) {
//!   case QPX_ACTION_PERSIST:   /* qpx_action_record(r, k, ...) for k < a.count: append + fsync,  */
//!                              /* then send qpx_action_then(r, k, ...) for k < a.then_count,     */
//!                              /* then, after draining, qpx_persisted(r, now, a.id)              */
//!   case QPX_ACTION_SEND:      /* send a.bytes to a.peer                                         */
//!   case QPX_ACTION_COMMIT:    /* apply qpx_action_entry(r, k, ...) for k < a.count, slot a.slot */
//!   case QPX_ACTION_COMMITTED: /* answer the client of proposal a.id                             */
//!   default: break;            /* new kinds may appear in minor versions                         */
//!   }
//! }
//! uint64_t deadline;
//! if (qpx_next_deadline(r, &deadline) == QPX_OK) { /* arm a timer; then qpx_handle_timeout */ }
//! ```
//!
//! `examples/smoke.c` in the crate is a complete three-replica program; CI runs it on Linux,
//! macOS, Windows, an Android emulator and an iOS simulator.
//!
//! # Contract
//!
//! (Also stated in `include/quepaxa.h`.)
//!
//! - Every function returns a `QPX_*` status (`0` = ok, positive = informational,
//!   negative = error) unless stated otherwise. [`qpx_status_str`] names it and
//!   [`qpx_last_error`] describes the most recent failing call on the calling thread.
//! - **Draining:** after any mutating call, call [`qpx_poll`] until it returns
//!   `QPX_EMPTY` before the next mutating call (`QPX_ERR_NOT_DRAINED` otherwise).
//!   `qpx_persisted` is itself a mutating call: collect persist ids while draining and
//!   acknowledge the highest one afterwards (it covers every earlier persist).
//! - **Persist before send:** never send a persist's `then` messages before its records
//!   are durable.
//! - **Borrowed memory:** `qpx_bytes` returned by [`qpx_poll`] and the `qpx_action_*`
//!   accessors borrow from the handle and stay valid until the next call on that handle;
//!   copy what you keep. Inputs are copied by the library; you keep ownership.
//! - **Threads:** a handle may move between threads, but calls on one handle must not
//!   overlap. Different handles are independent.
//! - **Panics** never cross the boundary: a panic inside the library returns
//!   `QPX_ERR_PANIC` and poisons the handle; every later call on it returns
//!   `QPX_ERR_POISONED`. Free it with [`qpx_replica_free`].
//!
//! # Versioning
//!
//! [`qpx_abi_version`] returns `(major << 16) | minor`; compare it with
//! `QPX_ABI_VERSION_MAJOR`/`MINOR` from the header you compiled against. Settings and
//! metrics are not structs: the configuration is an opaque object set option by option
//! ([`qpx_config_set`]) and metrics are read one by one ([`qpx_metric`]), so a new one is a
//! new constant, and an option or metric the library does not know is an explicit
//! `QPX_ERR_INVALID_ARGUMENT`. The only structs are outputs with a fixed layout (`qpx_bytes`,
//! `qpx_action`, `qpx_entry`). Before 1.0 a minor ABI bump may be incompatible; the
//! changelog says when.
#![expect(non_camel_case_types)]
// C naming for exported types
// Panics across the C ABI are caught, but production code should not rely on that.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::{ptr, slice};

use bytes::Bytes;
use quepaxa::{Action, Error, Input, NodeId, PersistId, Replica, Slot, Timestamp};

mod actions;
mod config;

pub use actions::*;
pub use config::*;

/// ABI major version: incompatible changes bump it. While it is 0 the ABI is unstable and
/// every minor bump may be incompatible.
pub const QPX_ABI_VERSION_MAJOR: u32 = 0;
/// ABI minor version: additive changes bump it (any change while the major is 0).
/// 0.2: `qpx_install_snapshot` may return `QPX_IGNORED`.
/// 0.3: size-aware `qpx_config_init`, `qpx_get_metrics`, `qpx_last_error`, strict draining.
/// 0.4: `qpx_config.leader_order` (appended; older layouts keep the default order).
/// 0.5: membership changes: `qpx_propose_reconfiguration`, `QPX_ACTION_RECONFIGURED`,
/// `qpx_action_member`, `QPX_ERR_RECONFIGURATION_PENDING`.
/// 0.6: `QPX_ACTION_RETIRED`.
/// 0.7: opaque `qpx_config` (`qpx_config_new`, `qpx_config_set`, `qpx_config_set_leader_order`,
/// `qpx_config_free`) and `qpx_metric` replace the `qpx_config` and `qpx_metrics` structs,
/// `qpx_config_init` and `qpx_get_metrics` (incompatible).
pub const QPX_ABI_VERSION_MINOR: u32 = 7;

/// Success.
pub const QPX_OK: i32 = 0;
/// No action, deadline or value to report.
pub const QPX_EMPTY: i32 = 1;
/// The output buffer is too small; the required length was written.
pub const QPX_BUFFER_TOO_SMALL: i32 = 2;
/// The call had no effect: the snapshot is older than what the replica already emitted.
pub const QPX_IGNORED: i32 = 3;
/// A null pointer, unknown option or metric, out-of-range value or index.
pub const QPX_ERR_INVALID_ARGUMENT: i32 = -1;
/// The configuration is invalid.
pub const QPX_ERR_INVALID_CONFIG: i32 = -2;
/// A durable record passed to `qpx_replica_new` is corrupt.
pub const QPX_ERR_CORRUPT_RECORD: i32 = -3;
/// Too many outstanding proposals.
pub const QPX_ERR_BUSY: i32 = -4;
/// The payload does not fit in one batch.
pub const QPX_ERR_PAYLOAD_TOO_LARGE: i32 = -5;
/// The replica fenced itself after a storage failure.
pub const QPX_ERR_FENCED: i32 = -6;
/// Actions from the previous call were not drained.
pub const QPX_ERR_NOT_DRAINED: i32 = -7;
/// Unknown or repeated persist id.
pub const QPX_ERR_UNKNOWN_PERSIST_ID: i32 = -8;
/// Malformed core state for `qpx_compact` / `qpx_install_snapshot`.
pub const QPX_ERR_INVALID_CORE_STATE: i32 = -9;
/// A membership change has not taken effect yet (ABI 0.5).
pub const QPX_ERR_RECONFIGURATION_PENDING: i32 = -10;
/// The library panicked; the handle is now poisoned.
pub const QPX_ERR_PANIC: i32 = -100;
/// The handle panicked earlier and is unusable.
pub const QPX_ERR_POISONED: i32 = -101;

/// Append `records` durably after earlier persists, then send each `then` message, then
/// call `qpx_persisted(id)`. If `QPX_PERSIST_REPLACE` is set in `flags`, atomically
/// replace all earlier records instead of appending.
pub const QPX_ACTION_PERSIST: u32 = 1;
/// Send `bytes` to `peer`.
pub const QPX_ACTION_SEND: u32 = 2;
/// Apply the `count` entries of `slot` (read them with `qpx_action_entry`).
pub const QPX_ACTION_COMMIT: u32 = 3;
/// Local proposal `id` committed at or before `slot`.
pub const QPX_ACTION_COMMITTED: u32 = 4;
/// Read barrier `id` is ready once commits up to `slot` are applied.
pub const QPX_ACTION_READ_READY: u32 = 5;
/// Fetch `peer`'s application snapshot covering `slot` and call `qpx_install_snapshot`.
pub const QPX_ACTION_SNAPSHOT_NEEDED: u32 = 6;
/// Local proposal `id` will not be reported again (`flags` = `QPX_DROP_*`). With
/// `QPX_DROP_FENCED` the outcome is unknown: it may still commit, as after a crash.
pub const QPX_ACTION_DROPPED: u32 = 7;
/// A membership change was applied: from slot `slot`, the `count` members (read them with
/// `qpx_action_member`, preferred leaders first) form epoch `id` (ABI 0.5).
pub const QPX_ACTION_RECONFIGURED: u32 = 8;
/// This replica is not a member of epoch `id`, and a majority of that epoch's members
/// durably know every slot before it: it may shut down, unless a later change added it
/// back (ABI 0.6).
pub const QPX_ACTION_RETIRED: u32 = 9;

/// `flags` bit of `QPX_ACTION_PERSIST`: replace all earlier records.
pub const QPX_PERSIST_REPLACE: u32 = 1;
/// `flags` of `QPX_ACTION_DROPPED`: the replica is fenced (outcome unknown).
pub const QPX_DROP_FENCED: u32 = 1;

/// A borrowed byte range. `ptr` may be null when `len` is 0.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct qpx_bytes {
    /// First byte.
    pub ptr: *const u8,
    /// Length in bytes.
    pub len: usize,
}

impl qpx_bytes {
    const EMPTY: Self = Self { ptr: ptr::null(), len: 0 };

    fn of(b: &[u8]) -> Self {
        Self { ptr: b.as_ptr(), len: b.len() }
    }
}

/// An opaque replica handle.
pub struct qpx_replica {
    replica: Replica,
    queue: VecDeque<Action>,
    current: Option<Action>,
    /// `qpx_poll` returned `QPX_EMPTY` since the last mutating call.
    drained: bool,
    poisoned: bool,
}

thread_local! {
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

/// An argument check failed before the call reached the replica.
fn invalid() -> i32 {
    set_last_error("invalid argument: null pointer or out-of-range index");
    QPX_ERR_INVALID_ARGUMENT
}

fn set_last_error(msg: &str) {
    // `try_with`: a call from a TLS destructor after ours ran loses the message, not the
    // process (a panic here would unwind across the C boundary).
    let _ = LAST_ERROR.try_with(|e| msg.clone_into(&mut e.borrow_mut()));
}

/// Records a failing status's description unless a more specific one was set.
fn note(rc: i32) -> i32 {
    if rc < 0 {
        let _ = LAST_ERROR.try_with(|e| {
            if e.borrow().is_empty() {
                // SAFETY: `qpx_status_str` returns a static NUL-terminated string.
                let s = unsafe { std::ffi::CStr::from_ptr(qpx_status_str(rc)) };
                *e.borrow_mut() = s.to_string_lossy().into_owned();
            }
        });
    }
    rc
}

impl qpx_replica {
    fn pull(&mut self) {
        let mut out = Vec::new();
        self.replica.poll_actions(&mut out);
        self.queue.extend(out);
    }
}

fn code(e: Error) -> i32 {
    set_last_error(&e.to_string());
    match e {
        Error::InvalidConfig(_) => QPX_ERR_INVALID_CONFIG,
        Error::CorruptRecord { .. } => QPX_ERR_CORRUPT_RECORD,
        Error::Busy => QPX_ERR_BUSY,
        Error::PayloadTooLarge => QPX_ERR_PAYLOAD_TOO_LARGE,
        Error::Fenced => QPX_ERR_FENCED,
        Error::NotDrained => QPX_ERR_NOT_DRAINED,
        Error::UnknownPersistId => QPX_ERR_UNKNOWN_PERSIST_ID,
        Error::InvalidCoreState => QPX_ERR_INVALID_CORE_STATE,
        Error::ReconfigurationPending => QPX_ERR_RECONFIGURATION_PENDING,
        _ => QPX_ERR_INVALID_ARGUMENT,
    }
}

/// Runs `f` on a live handle, converting panics and poisoning.
fn with(h: *mut qpx_replica, f: impl FnOnce(&mut qpx_replica) -> i32) -> i32 {
    set_last_error("");
    note(with_inner(h, f))
}

fn with_inner(h: *mut qpx_replica, f: impl FnOnce(&mut qpx_replica) -> i32) -> i32 {
    // SAFETY: the caller passes a handle from `qpx_replica_new` that is not used concurrently.
    let Some(h) = (unsafe { h.as_mut() }) else { return invalid() };
    if h.poisoned {
        return QPX_ERR_POISONED;
    }
    match catch_unwind(AssertUnwindSafe(|| f(h))) {
        Ok(rc) => rc,
        Err(_) => {
            h.poisoned = true;
            QPX_ERR_PANIC
        }
    }
}

/// A mutating call: refuses while actions are undrained, then queues new actions.
fn mutate(h: *mut qpx_replica, f: impl FnOnce(&mut Replica) -> Result<(), Error>) -> i32 {
    with(h, |h| {
        if !h.drained {
            return QPX_ERR_NOT_DRAINED;
        }
        h.current = None;
        let rc = match f(&mut h.replica) {
            Ok(()) => QPX_OK,
            Err(e) => code(e),
        };
        h.pull();
        // Nothing new to take means nothing to drain.
        h.drained = h.queue.is_empty();
        rc
    })
}

/// # Safety
/// `ptr` must point to `len` readable bytes (or `len` must be 0).
unsafe fn bytes_from(ptr: *const u8, len: usize) -> Option<Bytes> {
    if len == 0 {
        return Some(Bytes::new());
    }
    if ptr.is_null() {
        return None;
    }
    // SAFETY: guaranteed by the caller.
    Some(Bytes::copy_from_slice(unsafe { slice::from_raw_parts(ptr, len) }))
}

/// `(major << 16) | minor`.
#[unsafe(no_mangle)]
pub extern "C" fn qpx_abi_version() -> u32 {
    (QPX_ABI_VERSION_MAJOR << 16) | QPX_ABI_VERSION_MINOR
}

/// Static, NUL-terminated description of a status code.
#[unsafe(no_mangle)]
pub extern "C" fn qpx_status_str(status: i32) -> *const std::ffi::c_char {
    let s: &'static [u8] = match status {
        QPX_OK => b"ok\0",
        QPX_EMPTY => b"empty\0",
        QPX_BUFFER_TOO_SMALL => b"buffer too small\0",
        QPX_IGNORED => b"ignored: snapshot older than the emitted prefix\0",
        QPX_ERR_INVALID_ARGUMENT => b"invalid argument\0",
        QPX_ERR_INVALID_CONFIG => b"invalid config\0",
        QPX_ERR_CORRUPT_RECORD => b"corrupt durable record\0",
        QPX_ERR_BUSY => b"too many outstanding proposals\0",
        QPX_ERR_PAYLOAD_TOO_LARGE => b"payload too large\0",
        QPX_ERR_FENCED => b"replica fenced after a storage failure\0",
        QPX_ERR_NOT_DRAINED => b"actions not drained\0",
        QPX_ERR_UNKNOWN_PERSIST_ID => b"unknown persist id\0",
        QPX_ERR_INVALID_CORE_STATE => b"invalid core state\0",
        QPX_ERR_RECONFIGURATION_PENDING => b"a membership change has not taken effect yet\0",
        QPX_ERR_PANIC => b"internal panic; handle poisoned\0",
        QPX_ERR_POISONED => b"handle poisoned by an earlier panic\0",
        _ => b"unknown status\0",
    };
    s.as_ptr().cast()
}

/// Creates a replica with configuration `cfg` (copied; free it whenever) from
/// `n_records` durable records (in persisted order).
///
/// # Safety
/// `cfg` must come from `qpx_config_new`; `records` must point to `n_records` valid
/// `qpx_bytes`; `out` must be writable. On success `*out` owns a handle to release with `qpx_replica_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_replica_new(
    cfg: *const qpx_config,
    records: *const qpx_bytes,
    n_records: usize,
    now_ns: u64,
    out: *mut *mut qpx_replica,
) -> i32 {
    set_last_error("");
    let result = catch_unwind(|| {
        if out.is_null() {
            return invalid();
        }
        // SAFETY: guaranteed by the caller.
        let Some(cfg) = (unsafe { cfg.as_ref() }) else { return invalid() };
        let config = cfg.config.clone();
        let raw: &[qpx_bytes] = if n_records == 0 {
            &[]
        } else if records.is_null() {
            return invalid();
        } else {
            // SAFETY: guaranteed by the caller.
            unsafe { slice::from_raw_parts(records, n_records) }
        };
        let mut durable = Vec::with_capacity(raw.len());
        for r in raw {
            // SAFETY: guaranteed by the caller.
            let Some(b) = (unsafe { bytes_from(r.ptr, r.len) }) else { return invalid() };
            durable.push(b);
        }
        match Replica::new(config, durable, Timestamp(now_ns)) {
            Ok(replica) => {
                let mut h = Box::new(qpx_replica {
                    replica,
                    queue: VecDeque::new(),
                    current: None,
                    drained: false,
                    poisoned: false,
                });
                h.pull();
                // SAFETY: checked non-null above.
                unsafe { out.write(Box::into_raw(h)) };
                QPX_OK
            }
            Err(e) => code(e),
        }
    });
    note(result.unwrap_or(QPX_ERR_PANIC))
}

/// Releases a handle (null is ignored).
///
/// # Safety
/// `h` must come from `qpx_replica_new` and not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_replica_free(h: *mut qpx_replica) {
    if !h.is_null() {
        // SAFETY: the caller hands back ownership of a handle from `qpx_replica_new`.
        drop(unsafe { Box::from_raw(h) });
    }
}

/// Queues a write; its id is written to `out_id`.
///
/// # Safety
/// `h` must be a live handle; `payload` must point to `len` bytes; `out_id` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_propose(
    h: *mut qpx_replica,
    now_ns: u64,
    payload: *const u8,
    len: usize,
    out_id: *mut u64,
) -> i32 {
    // SAFETY: guaranteed by the caller.
    let Some(payload) = (unsafe { bytes_from(payload, len) }) else { return invalid() };
    if out_id.is_null() {
        return invalid();
    }
    mutate(h, |r| {
        let id = r.propose(Timestamp(now_ns), payload)?;
        // SAFETY: checked non-null; the caller guarantees it is writable.
        unsafe { out_id.write(id.0) };
        Ok(())
    })
}

/// Proposes new membership: `members_len` ids at `members`, with `order_len` ids at
/// `leader_order` as the preferred leaders (0 and null: by id). Its id is written to
/// `out_id` and reported like a proposal's; the change takes effect 64 slots after it is
/// decided (`QPX_ACTION_RECONFIGURED`). `QPX_ERR_RECONFIGURATION_PENDING` while another
/// change has not taken effect. A joining replica runs with the original configuration.
///
/// # Safety
/// `h` must be a live handle; `members` must point to `members_len` ids and `leader_order`
/// to `order_len` ids (either may be null when its length is 0); `out_id` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_propose_reconfiguration(
    h: *mut qpx_replica,
    now_ns: u64,
    members: *const u32,
    members_len: usize,
    leader_order: *const u32,
    order_len: usize,
    out_id: *mut u64,
) -> i32 {
    // SAFETY: guaranteed by the caller.
    let (Some(members), Some(order)) =
        (unsafe { ids_from(members, members_len) }, unsafe { ids_from(leader_order, order_len) })
    else {
        return invalid();
    };
    if out_id.is_null() {
        return invalid();
    }
    mutate(h, |r| {
        let id = r.propose_reconfiguration(Timestamp(now_ns), members, order)?;
        // SAFETY: checked non-null; the caller guarantees it is writable.
        unsafe { out_id.write(id.0) };
        Ok(())
    })
}

/// # Safety
/// `ptr` must point to `len` readable ids (or `len` must be 0).
unsafe fn ids_from(ptr: *const u32, len: usize) -> Option<Vec<NodeId>> {
    if len == 0 {
        return Some(Vec::new());
    }
    if ptr.is_null() {
        return None;
    }
    // SAFETY: guaranteed by the caller.
    Some(unsafe { slice::from_raw_parts(ptr, len) }.iter().copied().map(NodeId).collect())
}

/// Requests a linearizable read point; its id is written to `out_id`.
///
/// # Safety
/// `h` must be a live handle; `out_id` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_read_barrier(h: *mut qpx_replica, now_ns: u64, out_id: *mut u64) -> i32 {
    if out_id.is_null() {
        return invalid();
    }
    mutate(h, |r| {
        let id = r.read_barrier(Timestamp(now_ns))?;
        // SAFETY: checked non-null; the caller guarantees it is writable.
        unsafe { out_id.write(id.0) };
        Ok(())
    })
}

/// Feeds a network message exactly as received.
///
/// # Safety
/// `h` must be a live handle; `msg` must point to `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_handle_message(
    h: *mut qpx_replica,
    now_ns: u64,
    msg: *const u8,
    len: usize,
) -> i32 {
    // SAFETY: guaranteed by the caller.
    let Some(msg) = (unsafe { bytes_from(msg, len) }) else { return invalid() };
    mutate(h, |r| r.handle(Timestamp(now_ns), Input::Message(msg)))
}

/// Reports that every persist up to and including `id` is durable.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_persisted(h: *mut qpx_replica, now_ns: u64, id: u64) -> i32 {
    mutate(h, |r| r.handle(Timestamp(now_ns), Input::Persisted(PersistId(id))))
}

/// Reports that persist `id` failed; the replica fences itself.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_storage_failed(h: *mut qpx_replica, now_ns: u64, id: u64) -> i32 {
    mutate(h, |r| r.handle(Timestamp(now_ns), Input::StorageFailed(PersistId(id))))
}

/// Fires timers due at `now_ns`.
///
/// # Safety
/// `h` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_handle_timeout(h: *mut qpx_replica, now_ns: u64) -> i32 {
    mutate(h, |r| r.handle_timeout(Timestamp(now_ns)))
}

/// Writes the next timer deadline to `out_ns`, or returns `QPX_EMPTY` if none.
///
/// # Safety
/// `h` must be a live handle; `out_ns` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_next_deadline(h: *mut qpx_replica, out_ns: *mut u64) -> i32 {
    if out_ns.is_null() {
        return invalid();
    }
    with(h, |h| match h.replica.next_deadline() {
        Some(Timestamp(t)) => {
            // SAFETY: checked non-null; the caller guarantees it is writable.
            unsafe { out_ns.write(t) };
            QPX_OK
        }
        None => QPX_EMPTY,
    })
}

/// Number of contiguous decided slots.
///
/// # Safety
/// `h` must be a live handle; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_commit_index(h: *mut qpx_replica, out: *mut u64) -> i32 {
    if out.is_null() {
        return invalid();
    }
    with(h, |h| {
        // SAFETY: checked non-null; the caller guarantees it is writable.
        unsafe { out.write(h.replica.commit_index()) };
        QPX_OK
    })
}

/// Copies the replica's core state into `buf`. Two-call pattern: with too small a `cap`
/// it returns `QPX_BUFFER_TOO_SMALL` after writing the needed size to `out_len`.
///
/// # Safety
/// `h` must be a live handle; `buf` writable for `cap` bytes (or null with `cap` 0);
/// `out_len` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_core_state(
    h: *mut qpx_replica,
    buf: *mut u8,
    cap: usize,
    out_len: *mut usize,
) -> i32 {
    if out_len.is_null() || (buf.is_null() && cap > 0) {
        return invalid();
    }
    with(h, |h| {
        let state = h.replica.core_state();
        // SAFETY: checked non-null; the caller guarantees it is writable.
        unsafe { out_len.write(state.len()) };
        if cap < state.len() {
            return QPX_BUFFER_TOO_SMALL;
        }
        // SAFETY: `buf` has room for `cap >= state.len()` bytes.
        unsafe { ptr::copy_nonoverlapping(state.as_ptr(), buf, state.len()) };
        QPX_OK
    })
}

/// Discards log state up to `through`, snapshotted by the application with `state`.
///
/// # Safety
/// `h` must be a live handle; `state` must point to `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_compact(
    h: *mut qpx_replica,
    now_ns: u64,
    through: u64,
    state: *const u8,
    len: usize,
) -> i32 {
    // SAFETY: guaranteed by the caller.
    let Some(state) = (unsafe { bytes_from(state, len) }) else { return invalid() };
    mutate(h, |r| r.compact(Timestamp(now_ns), Slot(through), state))
}

/// Adopts a snapshot covering slots up to `through`; `QPX_IGNORED` if the replica already
/// emitted past it (do not rewind the application state in that case).
///
/// # Safety
/// `h` must be a live handle; `state` must point to `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_install_snapshot(
    h: *mut qpx_replica,
    now_ns: u64,
    through: u64,
    state: *const u8,
    len: usize,
) -> i32 {
    // SAFETY: guaranteed by the caller.
    let Some(state) = (unsafe { bytes_from(state, len) }) else { return invalid() };
    let mut adopted = true;
    let rc = mutate(h, |r| {
        adopted = r.install_snapshot(Timestamp(now_ns), Slot(through), state)?;
        Ok(())
    });
    if rc == QPX_OK && !adopted { QPX_IGNORED } else { rc }
}

/// Network messages rejected (malformed, foreign or from non-members).
pub const QPX_METRIC_MESSAGES_REJECTED: u32 = 1;
/// Record requests re-sent.
pub const QPX_METRIC_RETRANSMITS: u32 = 2;
/// Catch-up requests re-sent.
pub const QPX_METRIC_CATCH_UP_RETRIES: u32 = 3;
/// Slots learned as decided.
pub const QPX_METRIC_SLOTS_DECIDED: u32 = 4;
/// Own decisions on the leader fast path.
pub const QPX_METRIC_FAST_PATH_DECISIONS: u32 = 5;
/// Decided batches that failed to decode.
pub const QPX_METRIC_CORRUPT_BATCHES: u32 = 6;
/// Batches forwarded to the leader.
pub const QPX_METRIC_FORWARDED: u32 = 7;
/// Own entries proposed directly after the leader was too slow.
pub const QPX_METRIC_HEDGED: u32 = 8;
/// Gauge: decided slots held in memory.
pub const QPX_METRIC_RETAINED_SLOTS: u32 = 9;
/// Gauge: recorder registers held in memory.
pub const QPX_METRIC_LIVE_REGISTERS: u32 = 10;

/// Reads counter or gauge `metric` (`QPX_METRIC_*`, see the Rust `Metrics`) into `out`; an
/// unknown metric is `QPX_ERR_INVALID_ARGUMENT`.
///
/// # Safety
/// `h` must be a live handle; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_metric(h: *mut qpx_replica, metric: u32, out: *mut u64) -> i32 {
    if out.is_null() {
        return invalid();
    }
    with(h, |h| {
        let m = h.replica.metrics();
        let v = match metric {
            QPX_METRIC_MESSAGES_REJECTED => m.messages_rejected,
            QPX_METRIC_RETRANSMITS => m.retransmits,
            QPX_METRIC_CATCH_UP_RETRIES => m.catch_up_retries,
            QPX_METRIC_SLOTS_DECIDED => m.slots_decided,
            QPX_METRIC_FAST_PATH_DECISIONS => m.fast_path_decisions,
            QPX_METRIC_CORRUPT_BATCHES => m.corrupt_batches,
            QPX_METRIC_FORWARDED => m.forwarded,
            QPX_METRIC_HEDGED => m.hedged,
            QPX_METRIC_RETAINED_SLOTS => m.retained_slots,
            QPX_METRIC_LIVE_REGISTERS => m.live_registers,
            _ => {
                set_last_error(&format!("unknown metric {metric}"));
                return QPX_ERR_INVALID_ARGUMENT;
            }
        };
        // SAFETY: checked non-null; the caller guarantees it is writable.
        unsafe { out.write(v) };
        QPX_OK
    })
}

/// Copies the description of the calling thread's most recent failing call (empty if the
/// last call succeeded) as a NUL-terminated string. Two-call pattern: with too small a
/// `cap` it returns `QPX_BUFFER_TOO_SMALL` after writing the needed size (including the
/// NUL) to `out_len`.
///
/// # Safety
/// `buf` must be writable for `cap` bytes (or null with `cap` 0); `out_len` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_last_error(buf: *mut std::ffi::c_char, cap: usize, out_len: *mut usize) -> i32 {
    if out_len.is_null() || (buf.is_null() && cap > 0) {
        return QPX_ERR_INVALID_ARGUMENT;
    }
    let copy = |msg: &str| {
        let need = msg.len() + 1;
        // SAFETY: checked non-null; the caller guarantees it is writable.
        unsafe { out_len.write(need) };
        if cap < need {
            return QPX_BUFFER_TOO_SMALL;
        }
        // SAFETY: `buf` has room for `cap >= need` bytes.
        unsafe {
            ptr::copy_nonoverlapping(msg.as_ptr(), buf.cast::<u8>(), msg.len());
            buf.add(msg.len()).write(0);
        }
        QPX_OK
    };
    LAST_ERROR.try_with(|e| copy(&e.borrow())).unwrap_or_else(|_| copy(""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use quepaxa::Config;

    #[test]
    fn a_panic_poisons_the_handle() {
        let cfg = Config::new([1; 16], NodeId(0), vec![NodeId(0)], [1; 32]);
        let replica = Replica::new(cfg, [], Timestamp(0)).unwrap();
        let h = Box::into_raw(Box::new(qpx_replica {
            replica,
            queue: VecDeque::new(),
            current: None,
            drained: false,
            poisoned: false,
        }));
        assert_eq!(with(h, |_| panic!("boom")), QPX_ERR_PANIC);
        assert_eq!(with(h, |_| QPX_OK), QPX_ERR_POISONED);
        // SAFETY: `h` came from `Box::into_raw` above.
        unsafe { qpx_replica_free(h) };
    }
}

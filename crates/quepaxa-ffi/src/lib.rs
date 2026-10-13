//! Stable C ABI over the `quepaxa` core.
//!
//! Contract (also in `include/quepaxa.h`):
//! - Every function returns a `QPX_*` status (`0` = ok, positive = informational,
//!   negative = error) unless stated otherwise.
//! - A handle may move between threads, but calls on one handle must not overlap.
//! - Pointers returned by `qpx_poll` and the `qpx_action_*` accessors borrow from the
//!   handle and stay valid until the next call on that handle.
//! - After any mutating call, drain actions with `qpx_poll` until `QPX_EMPTY` before the
//!   next mutating call (`QPX_ERR_NOT_DRAINED` otherwise).
//! - A panic inside the library returns `QPX_ERR_PANIC` and poisons the handle; every
//!   later call on it returns `QPX_ERR_POISONED`.
#![allow(non_camel_case_types)] // C naming for exported types

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::{ptr, slice};

use bytes::Bytes;
use quepaxa::{Action, Config, DropReason, Error, Input, NodeId, PersistId, Replica, Slot, Timestamp};

/// ABI major version: incompatible changes bump it.
pub const QPX_ABI_VERSION_MAJOR: u32 = 0;
/// ABI minor version: additive changes bump it.
pub const QPX_ABI_VERSION_MINOR: u32 = 1;

/// Success.
pub const QPX_OK: i32 = 0;
/// No action, deadline or value to report.
pub const QPX_EMPTY: i32 = 1;
/// The output buffer is too small; the required length was written.
pub const QPX_BUFFER_TOO_SMALL: i32 = 2;
/// The call had no effect: the snapshot is older than what the replica already emitted.
pub const QPX_IGNORED: i32 = 3;
/// A null pointer, bad `struct_size` or out-of-range index.
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
/// Local proposal `id` will never commit (`flags` = `QPX_DROP_*`).
pub const QPX_ACTION_DROPPED: u32 = 7;

/// `flags` bit of `QPX_ACTION_PERSIST`: replace all earlier records.
pub const QPX_PERSIST_REPLACE: u32 = 1;
/// `flags` of `QPX_ACTION_DROPPED`: the replica is fenced.
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

/// Replica configuration. Initialize with `qpx_config_init`, then set fields.
#[repr(C)]
pub struct qpx_config {
    /// `sizeof(qpx_config)`, set by `qpx_config_init`; lets the struct grow compatibly.
    pub struct_size: usize,
    /// Cluster identity; messages from other clusters are dropped.
    pub cluster_id: [u8; 16],
    /// This replica.
    pub id: u32,
    /// All replicas including this one.
    pub members: *const u32,
    /// Number of `members`.
    pub members_len: usize,
    /// Fresh CSPRNG output on every start (never reuse it).
    pub seed: [u8; 32],
    /// See the Rust `Config` for the meaning of the remaining fields.
    pub max_batch_bytes: u32,
    /// Largest accepted message.
    pub max_message_bytes: u32,
    /// Concurrent slots driven by this replica (≤ 64).
    pub max_in_flight_slots: u32,
    /// Outstanding proposals before `QPX_ERR_BUSY`.
    pub max_outstanding_proposals: u32,
    /// Record request retransmission interval, nanoseconds.
    pub retransmit_interval_ns: u64,
    /// Lower bound of the hedge delay, nanoseconds.
    pub hedge_min_ns: u64,
    /// Upper bound of the hedge delay, nanoseconds.
    pub hedge_max_ns: u64,
}

/// One action. Borrowed fields stay valid until the next call on the handle.
#[repr(C)]
pub struct qpx_action {
    /// `QPX_ACTION_*`.
    pub kind: u32,
    /// `QPX_PERSIST_REPLACE` or `QPX_DROP_*`.
    pub flags: u32,
    /// Persist id or proposal id.
    pub id: u64,
    /// Slot of a commit, committed, read-ready or snapshot action.
    pub slot: u64,
    /// Destination of a send, or the peer holding a needed snapshot.
    pub peer: u32,
    /// Message of a send.
    pub bytes: qpx_bytes,
    /// Records of a persist, or entries of a commit.
    pub count: usize,
    /// Messages gated on a persist.
    pub then_count: usize,
}

/// A committed entry.
#[repr(C)]
pub struct qpx_entry {
    /// Replica that accepted the proposal.
    pub origin: u32,
    /// Its incarnation at the time.
    pub incarnation: u64,
    /// The proposal id at the origin.
    pub id: u64,
    /// Application bytes.
    pub payload: qpx_bytes,
}

/// An opaque replica handle.
pub struct qpx_replica {
    replica: Replica,
    queue: VecDeque<Action>,
    current: Option<Action>,
    poisoned: bool,
}

impl qpx_replica {
    fn pull(&mut self) {
        let mut out = Vec::new();
        self.replica.poll_actions(&mut out);
        self.queue.extend(out);
    }
}

fn code(e: Error) -> i32 {
    match e {
        Error::InvalidConfig(_) => QPX_ERR_INVALID_CONFIG,
        Error::CorruptRecord { .. } => QPX_ERR_CORRUPT_RECORD,
        Error::Busy => QPX_ERR_BUSY,
        Error::PayloadTooLarge => QPX_ERR_PAYLOAD_TOO_LARGE,
        Error::Fenced => QPX_ERR_FENCED,
        Error::NotDrained => QPX_ERR_NOT_DRAINED,
        Error::UnknownPersistId => QPX_ERR_UNKNOWN_PERSIST_ID,
        Error::InvalidCoreState => QPX_ERR_INVALID_CORE_STATE,
        _ => QPX_ERR_INVALID_ARGUMENT,
    }
}

/// Runs `f` on a live handle, converting panics and poisoning.
fn with(h: *mut qpx_replica, f: impl FnOnce(&mut qpx_replica) -> i32) -> i32 {
    // SAFETY: the caller passes a handle from `qpx_replica_new` that is not used concurrently.
    let Some(h) = (unsafe { h.as_mut() }) else { return QPX_ERR_INVALID_ARGUMENT };
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
        if !h.queue.is_empty() {
            return QPX_ERR_NOT_DRAINED;
        }
        h.current = None;
        let rc = match f(&mut h.replica) {
            Ok(()) => QPX_OK,
            Err(e) => code(e),
        };
        h.pull();
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
        QPX_ERR_PANIC => b"internal panic; handle poisoned\0",
        QPX_ERR_POISONED => b"handle poisoned by an earlier panic\0",
        _ => b"unknown status\0",
    };
    s.as_ptr().cast()
}

/// Fills `cfg` with defaults (including `struct_size`); `members` stays null.
///
/// # Safety
/// `cfg` must be null or point to writable memory for a `qpx_config`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_config_init(cfg: *mut qpx_config) -> i32 {
    if cfg.is_null() {
        return QPX_ERR_INVALID_ARGUMENT;
    }
    let d = Config::new([0; 16], NodeId(0), Vec::new(), [0; 32]);
    let init = qpx_config {
        struct_size: size_of::<qpx_config>(),
        cluster_id: [0; 16],
        id: 0,
        members: ptr::null(),
        members_len: 0,
        seed: [0; 32],
        max_batch_bytes: d.max_batch_bytes,
        max_message_bytes: d.max_message_bytes,
        max_in_flight_slots: d.max_in_flight_slots,
        max_outstanding_proposals: d.max_outstanding_proposals,
        retransmit_interval_ns: d.retransmit_interval,
        hedge_min_ns: d.hedge_min,
        hedge_max_ns: d.hedge_max,
    };
    // SAFETY: checked non-null; the caller guarantees it is writable.
    unsafe { cfg.write(init) };
    QPX_OK
}

/// Creates a replica from `n_records` durable records (in persisted order).
///
/// # Safety
/// `cfg` must point to an initialized `qpx_config` whose `members` points to
/// `members_len` ids; `records` must point to `n_records` valid `qpx_bytes`; `out` must
/// be writable. On success `*out` owns a handle to release with `qpx_replica_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_replica_new(
    cfg: *const qpx_config,
    records: *const qpx_bytes,
    n_records: usize,
    now_ns: u64,
    out: *mut *mut qpx_replica,
) -> i32 {
    let result = catch_unwind(|| {
        // SAFETY: guaranteed by the caller.
        let (Some(c), false) = (unsafe { cfg.as_ref() }, out.is_null()) else {
            return QPX_ERR_INVALID_ARGUMENT;
        };
        if c.struct_size < size_of::<qpx_config>() || (c.members.is_null() && c.members_len > 0) {
            return QPX_ERR_INVALID_ARGUMENT;
        }
        let members = if c.members_len == 0 {
            Vec::new()
        } else {
            // SAFETY: guaranteed by the caller.
            unsafe { slice::from_raw_parts(c.members, c.members_len) }.iter().copied().map(NodeId).collect()
        };
        let mut config = Config::new(c.cluster_id, NodeId(c.id), members, c.seed);
        config.max_batch_bytes = c.max_batch_bytes;
        config.max_message_bytes = c.max_message_bytes;
        config.max_in_flight_slots = c.max_in_flight_slots;
        config.max_outstanding_proposals = c.max_outstanding_proposals;
        config.retransmit_interval = c.retransmit_interval_ns;
        config.hedge_min = c.hedge_min_ns;
        config.hedge_max = c.hedge_max_ns;
        let raw: &[qpx_bytes] = if n_records == 0 {
            &[]
        } else if records.is_null() {
            return QPX_ERR_INVALID_ARGUMENT;
        } else {
            // SAFETY: guaranteed by the caller.
            unsafe { slice::from_raw_parts(records, n_records) }
        };
        let mut durable = Vec::with_capacity(raw.len());
        for r in raw {
            // SAFETY: guaranteed by the caller.
            let Some(b) = (unsafe { bytes_from(r.ptr, r.len) }) else { return QPX_ERR_INVALID_ARGUMENT };
            durable.push(b);
        }
        match Replica::new(config, durable, Timestamp(now_ns)) {
            Ok(replica) => {
                let mut h =
                    Box::new(qpx_replica { replica, queue: VecDeque::new(), current: None, poisoned: false });
                h.pull();
                // SAFETY: checked non-null above.
                unsafe { out.write(Box::into_raw(h)) };
                QPX_OK
            }
            Err(e) => code(e),
        }
    });
    result.unwrap_or(QPX_ERR_PANIC)
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
    let Some(payload) = (unsafe { bytes_from(payload, len) }) else { return QPX_ERR_INVALID_ARGUMENT };
    if out_id.is_null() {
        return QPX_ERR_INVALID_ARGUMENT;
    }
    mutate(h, |r| {
        let id = r.propose(Timestamp(now_ns), payload)?;
        // SAFETY: checked non-null; the caller guarantees it is writable.
        unsafe { out_id.write(id.0) };
        Ok(())
    })
}

/// Requests a linearizable read point; its id is written to `out_id`.
///
/// # Safety
/// `h` must be a live handle; `out_id` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_read_barrier(h: *mut qpx_replica, now_ns: u64, out_id: *mut u64) -> i32 {
    if out_id.is_null() {
        return QPX_ERR_INVALID_ARGUMENT;
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
    let Some(msg) = (unsafe { bytes_from(msg, len) }) else { return QPX_ERR_INVALID_ARGUMENT };
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
        return QPX_ERR_INVALID_ARGUMENT;
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
        return QPX_ERR_INVALID_ARGUMENT;
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
        return QPX_ERR_INVALID_ARGUMENT;
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
    let Some(state) = (unsafe { bytes_from(state, len) }) else { return QPX_ERR_INVALID_ARGUMENT };
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
    let Some(state) = (unsafe { bytes_from(state, len) }) else { return QPX_ERR_INVALID_ARGUMENT };
    let mut adopted = true;
    let rc = mutate(h, |r| {
        adopted = r.install_snapshot(Timestamp(now_ns), Slot(through), state)?;
        Ok(())
    });
    if rc == QPX_OK && !adopted { QPX_IGNORED } else { rc }
}

/// Takes the next action into `out`, or returns `QPX_EMPTY`.
///
/// # Safety
/// `h` must be a live handle; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_poll(h: *mut qpx_replica, out: *mut qpx_action) -> i32 {
    if out.is_null() {
        return QPX_ERR_INVALID_ARGUMENT;
    }
    with(h, |h| {
        h.current = h.queue.pop_front();
        let Some(a) = &h.current else { return QPX_EMPTY };
        let mut v = qpx_action {
            kind: 0,
            flags: 0,
            id: 0,
            slot: 0,
            peer: 0,
            bytes: qpx_bytes::EMPTY,
            count: 0,
            then_count: 0,
        };
        match a {
            Action::Persist { id, records, then, replace } => {
                v.kind = QPX_ACTION_PERSIST;
                v.id = id.0;
                v.count = records.len();
                v.then_count = then.len();
                v.flags = if *replace { QPX_PERSIST_REPLACE } else { 0 };
            }
            Action::Send(o) => {
                v.kind = QPX_ACTION_SEND;
                v.peer = o.to.0;
                v.bytes = qpx_bytes::of(&o.bytes);
            }
            Action::Commit { slot, entries } => {
                v.kind = QPX_ACTION_COMMIT;
                v.slot = slot.0;
                v.count = entries.len();
            }
            Action::Committed { id, slot } => {
                v.kind = QPX_ACTION_COMMITTED;
                v.id = id.0;
                v.slot = slot.0;
            }
            Action::ReadReady { id, slot } => {
                v.kind = QPX_ACTION_READ_READY;
                v.id = id.0;
                v.slot = slot.0;
            }
            Action::SnapshotNeeded { peer, through } => {
                v.kind = QPX_ACTION_SNAPSHOT_NEEDED;
                v.peer = peer.0;
                v.slot = through.0;
            }
            Action::Dropped { id, reason } => {
                v.kind = QPX_ACTION_DROPPED;
                v.id = id.0;
                v.flags = match reason {
                    DropReason::Fenced => QPX_DROP_FENCED,
                    _ => 0,
                };
            }
            _ => {}
        }
        // SAFETY: checked non-null; the caller guarantees it is writable.
        unsafe { out.write(v) };
        QPX_OK
    })
}

/// Record `i` of the current persist action.
///
/// # Safety
/// `h` must be a live handle; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_action_record(h: *mut qpx_replica, i: usize, out: *mut qpx_bytes) -> i32 {
    if out.is_null() {
        return QPX_ERR_INVALID_ARGUMENT;
    }
    with(h, |h| match &h.current {
        Some(Action::Persist { records, .. }) if i < records.len() => {
            // SAFETY: checked non-null; the caller guarantees it is writable.
            unsafe { out.write(qpx_bytes::of(&records[i])) };
            QPX_OK
        }
        _ => QPX_ERR_INVALID_ARGUMENT,
    })
}

/// Message `i` gated on the current persist action, and its destination.
///
/// # Safety
/// `h` must be a live handle; `to` and `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_action_then(
    h: *mut qpx_replica,
    i: usize,
    to: *mut u32,
    out: *mut qpx_bytes,
) -> i32 {
    if out.is_null() || to.is_null() {
        return QPX_ERR_INVALID_ARGUMENT;
    }
    with(h, |h| match &h.current {
        Some(Action::Persist { then, .. }) if i < then.len() => {
            // SAFETY: checked non-null; the caller guarantees both are writable.
            unsafe {
                to.write(then[i].to.0);
                out.write(qpx_bytes::of(&then[i].bytes));
            }
            QPX_OK
        }
        _ => QPX_ERR_INVALID_ARGUMENT,
    })
}

/// Entry `i` of the current commit action.
///
/// # Safety
/// `h` must be a live handle; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_action_entry(h: *mut qpx_replica, i: usize, out: *mut qpx_entry) -> i32 {
    if out.is_null() {
        return QPX_ERR_INVALID_ARGUMENT;
    }
    with(h, |h| match &h.current {
        Some(Action::Commit { entries, .. }) if i < entries.len() => {
            let e = &entries[i];
            let v = qpx_entry {
                origin: e.origin.0,
                incarnation: e.incarnation,
                id: e.id.0,
                payload: qpx_bytes::of(&e.payload),
            };
            // SAFETY: checked non-null; the caller guarantees it is writable.
            unsafe { out.write(v) };
            QPX_OK
        }
        _ => QPX_ERR_INVALID_ARGUMENT,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_poisons_the_handle() {
        let cfg = Config::new([1; 16], NodeId(0), vec![NodeId(0)], [1; 32]);
        let replica = Replica::new(cfg, [], Timestamp(0)).unwrap();
        let h = Box::into_raw(Box::new(qpx_replica {
            replica,
            queue: VecDeque::new(),
            current: None,
            poisoned: false,
        }));
        assert_eq!(with(h, |_| panic!("boom")), QPX_ERR_PANIC);
        assert_eq!(with(h, |_| QPX_OK), QPX_ERR_POISONED);
        // SAFETY: `h` came from `Box::into_raw` above.
        unsafe { qpx_replica_free(h) };
    }
}

//! Reading actions: `qpx_poll` and the accessors for the current action's parts.

use quepaxa::{Action, DropReason};

use crate::*;

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

/// Takes the next action into `out`, or returns `QPX_EMPTY`.
///
/// # Safety
/// `h` must be a live handle; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_poll(h: *mut qpx_replica, out: *mut qpx_action) -> i32 {
    if out.is_null() {
        return invalid();
    }
    with(h, |h| {
        h.current = h.queue.pop_front();
        let Some(a) = &h.current else {
            h.drained = true;
            return QPX_EMPTY;
        };
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
            Action::Reconfigured { epoch, from, members } => {
                v.kind = QPX_ACTION_RECONFIGURED;
                v.id = *epoch;
                v.slot = from.0;
                v.count = members.len();
            }
            Action::Retired { epoch } => {
                v.kind = QPX_ACTION_RETIRED;
                v.id = *epoch;
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
        return invalid();
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
        return invalid();
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

/// Member `i` of the current `QPX_ACTION_RECONFIGURED` action (preferred leaders first).
///
/// # Safety
/// `h` must be a live handle; `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_action_member(h: *mut qpx_replica, i: usize, out: *mut u32) -> i32 {
    if out.is_null() {
        return invalid();
    }
    with(h, |h| match &h.current {
        Some(Action::Reconfigured { members, .. }) if i < members.len() => {
            // SAFETY: checked non-null; the caller guarantees it is writable.
            unsafe { out.write(members[i].0) };
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
        return invalid();
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

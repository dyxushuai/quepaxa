//! The replica configuration: an opaque object built with setters, so no configuration
//! layout is part of the ABI and a new setting is a new option, not a new struct.

use std::ptr;

use quepaxa::{Config, NodeId};

use crate::{QPX_ERR_INVALID_ARGUMENT, QPX_OK, ids_from, invalid, set_last_error};

/// Replica configuration: create it with `qpx_config_new`, adjust it with `qpx_config_set`
/// and `qpx_config_set_leader_order`, pass it to `qpx_replica_new` (which copies it), and
/// release it with `qpx_config_free`.
pub struct qpx_config {
    pub(crate) config: Config,
}

/// Largest batch of entries per slot, bytes (`u32`).
pub const QPX_CONFIG_MAX_BATCH_BYTES: u32 = 1;
/// Largest accepted message, bytes (`u32`).
pub const QPX_CONFIG_MAX_MESSAGE_BYTES: u32 = 2;
/// Concurrent slots driven by this replica (`u32`, at most 64).
pub const QPX_CONFIG_MAX_IN_FLIGHT_SLOTS: u32 = 3;
/// Outstanding proposals before `QPX_ERR_BUSY` (`u32`).
pub const QPX_CONFIG_MAX_OUTSTANDING_PROPOSALS: u32 = 4;
/// Record request retransmission interval, nanoseconds.
pub const QPX_CONFIG_RETRANSMIT_INTERVAL_NS: u32 = 5;
/// Lower bound of the hedge delay, nanoseconds.
pub const QPX_CONFIG_HEDGE_MIN_NS: u32 = 6;
/// Upper bound of the hedge delay, nanoseconds.
pub const QPX_CONFIG_HEDGE_MAX_NS: u32 = 7;

/// Creates a configuration with default settings: `cluster_id` (16 bytes; messages from
/// other clusters are dropped), this replica's `id`, the `members_len` configured members at
/// `members` (including this one; a joining replica lists the original members), and
/// `seed` (32 bytes of fresh CSPRNG output on every start; never reuse it).
///
/// # Safety
/// `cluster_id` must point to 16 readable bytes, `seed` to 32, `members` to `members_len`
/// ids (or be null when it is 0); `out` must be writable. On success `*out` owns a
/// configuration to release with `qpx_config_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_config_new(
    cluster_id: *const u8,
    id: u32,
    members: *const u32,
    members_len: usize,
    seed: *const u8,
    out: *mut *mut qpx_config,
) -> i32 {
    set_last_error("");
    if cluster_id.is_null() || seed.is_null() || out.is_null() {
        return invalid();
    }
    // SAFETY: guaranteed by the caller.
    let Some(members) = (unsafe { ids_from(members, members_len) }) else { return invalid() };
    let (mut c, mut s) = ([0u8; 16], [0u8; 32]);
    // SAFETY: the caller guarantees 16 and 32 readable bytes.
    unsafe {
        ptr::copy_nonoverlapping(cluster_id, c.as_mut_ptr(), c.len());
        ptr::copy_nonoverlapping(seed, s.as_mut_ptr(), s.len());
    }
    let cfg = Box::new(qpx_config { config: Config::new(c, NodeId(id), members, s) });
    // SAFETY: checked non-null; the caller guarantees it is writable.
    unsafe { out.write(Box::into_raw(cfg)) };
    QPX_OK
}

fn narrow(field: &mut u32, value: u64) -> bool {
    u32::try_from(value).map(|v| *field = v).is_ok()
}

/// Sets option `option` (`QPX_CONFIG_*`) to `value`. An unknown option, or a value too large
/// for it, is `QPX_ERR_INVALID_ARGUMENT`; whether the settings are consistent is checked by
/// `qpx_replica_new` (`QPX_ERR_INVALID_CONFIG`).
///
/// # Safety
/// `cfg` must come from `qpx_config_new` and not be used concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_config_set(cfg: *mut qpx_config, option: u32, value: u64) -> i32 {
    set_last_error("");
    // SAFETY: guaranteed by the caller.
    let Some(cfg) = (unsafe { cfg.as_mut() }) else { return invalid() };
    let c = &mut cfg.config;
    let fits = match option {
        QPX_CONFIG_MAX_BATCH_BYTES => narrow(&mut c.max_batch_bytes, value),
        QPX_CONFIG_MAX_MESSAGE_BYTES => narrow(&mut c.max_message_bytes, value),
        QPX_CONFIG_MAX_IN_FLIGHT_SLOTS => narrow(&mut c.max_in_flight_slots, value),
        QPX_CONFIG_MAX_OUTSTANDING_PROPOSALS => narrow(&mut c.max_outstanding_proposals, value),
        QPX_CONFIG_RETRANSMIT_INTERVAL_NS => {
            c.retransmit_interval = value;
            true
        }
        QPX_CONFIG_HEDGE_MIN_NS => {
            c.hedge_min = value;
            true
        }
        QPX_CONFIG_HEDGE_MAX_NS => {
            c.hedge_max = value;
            true
        }
        _ => {
            set_last_error(&format!("unknown config option {option}"));
            return QPX_ERR_INVALID_ARGUMENT;
        }
    };
    if !fits {
        set_last_error(&format!("value {value} is out of range for config option {option}"));
        return QPX_ERR_INVALID_ARGUMENT;
    }
    QPX_OK
}

/// Sets the preferred leaders, first to last: every member once, or none (`len` 0) for
/// "by id". Every member must use the same order.
///
/// # Safety
/// `cfg` must come from `qpx_config_new` and not be used concurrently; `order` must point
/// to `len` ids (or be null when it is 0).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_config_set_leader_order(
    cfg: *mut qpx_config,
    order: *const u32,
    len: usize,
) -> i32 {
    set_last_error("");
    // SAFETY: guaranteed by the caller.
    let (Some(cfg), Some(order)) = (unsafe { cfg.as_mut() }, unsafe { ids_from(order, len) }) else {
        return invalid();
    };
    cfg.config.leader_order = order;
    QPX_OK
}

/// Releases a configuration (null is ignored). Replicas created from it are unaffected.
///
/// # Safety
/// `cfg` must come from `qpx_config_new` and not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn qpx_config_free(cfg: *mut qpx_config) {
    if !cfg.is_null() {
        // SAFETY: the caller hands back ownership of a configuration from `qpx_config_new`.
        drop(unsafe { Box::from_raw(cfg) });
    }
}

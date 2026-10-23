//! Exercises the C ABI from Rust (also run under Miri to catch undefined behaviour).

use std::ptr;

use quepaxa_ffi::*;

const N: usize = 3;

/// A configuration for replica `id` of `members`; free it with `qpx_config_free`.
fn config(id: u32, members: &[u32]) -> *mut qpx_config {
    let mut cfg = ptr::null_mut();
    let seed = [id as u8 + 1; 32];
    let rc = unsafe {
        qpx_config_new(
            b"rust-c-abi-tests".as_ptr(),
            id,
            members.as_ptr(),
            members.len(),
            seed.as_ptr(),
            &mut cfg,
        )
    };
    assert_eq!(rc, QPX_OK);
    cfg
}

/// `qpx_replica_new` from `cfg` with no durable records, then frees `cfg`.
fn replica_from(cfg: *mut qpx_config, records: &[qpx_bytes]) -> (i32, *mut qpx_replica) {
    let mut h = ptr::null_mut();
    let rc = unsafe { qpx_replica_new(cfg, records.as_ptr(), records.len(), 0, &mut h) };
    unsafe { qpx_config_free(cfg) };
    (rc, h)
}

unsafe fn new_replica(i: u32, members: &[u32; N]) -> *mut qpx_replica {
    let (rc, h) = replica_from(config(i, members), &[]);
    assert_eq!(rc, QPX_OK);
    h
}

fn empty_bytes() -> qpx_bytes {
    qpx_bytes { ptr: ptr::null(), len: 0 }
}

fn empty_action() -> qpx_action {
    qpx_action { kind: 0, flags: 0, id: 0, slot: 0, peer: 0, bytes: empty_bytes(), count: 0, then_count: 0 }
}

fn copy(b: qpx_bytes) -> Vec<u8> {
    if b.len == 0 {
        return Vec::new();
    }
    // SAFETY: the library returned a valid borrowed range, used before the next call.
    unsafe { std::slice::from_raw_parts(b.ptr, b.len) }.to_vec()
}

/// Executes every action of replica `i`, collecting committed payloads into `out`.
unsafe fn drain(r: &[*mut qpx_replica], i: usize, net: &mut Vec<(u32, Vec<u8>)>, out: &mut Vec<Vec<u8>>) {
    let mut last_persist = None;
    loop {
        let mut a = empty_action();
        let rc = unsafe { qpx_poll(r[i], &mut a) };
        if rc == QPX_EMPTY {
            break;
        }
        assert_eq!(rc, QPX_OK);
        match a.kind {
            QPX_ACTION_SEND => net.push((a.peer, copy(a.bytes))),
            QPX_ACTION_PERSIST => {
                for k in 0..a.then_count {
                    let (mut to, mut b) = (0, empty_bytes());
                    assert_eq!(unsafe { qpx_action_then(r[i], k, &mut to, &mut b) }, QPX_OK);
                    net.push((to, copy(b)));
                }
                last_persist = Some(a.id);
            }
            QPX_ACTION_COMMIT => {
                for k in 0..a.count {
                    let mut e = qpx_entry { origin: 0, incarnation: 0, id: 0, payload: empty_bytes() };
                    assert_eq!(unsafe { qpx_action_entry(r[i], k, &mut e) }, QPX_OK);
                    out.push(copy(e.payload));
                }
            }
            QPX_ACTION_RECONFIGURED => {
                let mut members = Vec::new();
                for k in 0..a.count {
                    let mut m = 0;
                    assert_eq!(unsafe { qpx_action_member(r[i], k, &mut m) }, QPX_OK);
                    members.push(m);
                }
                let mut m = 0;
                assert_eq!(unsafe { qpx_action_member(r[i], a.count, &mut m) }, QPX_ERR_INVALID_ARGUMENT);
                out.push(format!("epoch {} from {} {members:?}", a.id, a.slot).into_bytes());
            }
            QPX_ACTION_RETIRED => out.push(format!("retired {}", a.id).into_bytes()),
            _ => {}
        }
    }
    if let Some(id) = last_persist {
        assert_eq!(unsafe { qpx_persisted(r[i], 0, id) }, QPX_OK);
        unsafe { drain(r, i, net, out) };
    }
}

#[test]
fn three_replicas_commit_through_the_c_abi() {
    let members = [0, 1, 2];
    let r: Vec<_> = (0..N as u32).map(|i| unsafe { new_replica(i, &members) }).collect();
    let mut net = Vec::new();
    let mut commits = vec![Vec::new(); N];
    for (i, c) in commits.iter_mut().enumerate() {
        unsafe { drain(&r, i, &mut net, c) };
    }
    let mut id = 0;
    assert_eq!(unsafe { qpx_propose(r[2], 0, b"hi".as_ptr(), 2, &mut id) }, QPX_OK);
    unsafe { drain(&r, 2, &mut net, &mut commits[2]) };
    while !net.is_empty() {
        let (to, m) = net.remove(0);
        assert_eq!(unsafe { qpx_handle_message(r[to as usize], 0, m.as_ptr(), m.len()) }, QPX_OK);
        unsafe { drain(&r, to as usize, &mut net, &mut commits[to as usize]) };
    }
    for (i, c) in commits.iter().enumerate() {
        assert_eq!(c, &vec![b"hi".to_vec()], "replica {i}");
        let mut ci = 0;
        assert_eq!(unsafe { qpx_commit_index(r[i], &mut ci) }, QPX_OK);
        assert_eq!(ci, 1);
    }
    for h in r {
        unsafe { qpx_replica_free(h) };
    }
}

#[test]
fn contract_violations_are_reported_not_undefined() {
    let members = [0, 1, 2];
    let h = unsafe { new_replica(0, &members) };
    let mut id = 0;
    // The incarnation persist is still undrained.
    assert_eq!(unsafe { qpx_propose(h, 0, ptr::null(), 0, &mut id) }, QPX_ERR_NOT_DRAINED);
    let mut a = empty_action();
    // Taking the last action is not enough: the poll must report QPX_EMPTY.
    assert_eq!(unsafe { qpx_poll(h, &mut a) }, QPX_OK);
    assert_eq!(unsafe { qpx_propose(h, 0, ptr::null(), 0, &mut id) }, QPX_ERR_NOT_DRAINED);
    while unsafe { qpx_poll(h, &mut a) } == QPX_OK {}
    assert_eq!(unsafe { qpx_propose(h, 0, ptr::null(), 5, &mut id) }, QPX_ERR_INVALID_ARGUMENT);
    assert_eq!(unsafe { qpx_propose(h, 0, ptr::null(), 0, ptr::null_mut()) }, QPX_ERR_INVALID_ARGUMENT);
    assert_eq!(unsafe { qpx_poll(ptr::null_mut(), &mut a) }, QPX_ERR_INVALID_ARGUMENT);
    assert_eq!(unsafe { qpx_persisted(h, 0, 99) }, QPX_ERR_UNKNOWN_PERSIST_ID);
    let mut b = empty_bytes();
    assert_eq!(unsafe { qpx_action_record(h, 0, &mut b) }, QPX_ERR_INVALID_ARGUMENT);
    let mut len = 0;
    assert_eq!(unsafe { qpx_core_state(h, ptr::null_mut(), 0, &mut len) }, QPX_BUFFER_TOO_SMALL);
    let mut buf = vec![0u8; len];
    assert_eq!(unsafe { qpx_core_state(h, buf.as_mut_ptr(), buf.len(), &mut len) }, QPX_OK);
    assert_eq!(unsafe { qpx_compact(h, 0, 0, buf.as_ptr(), buf.len()) }, QPX_ERR_INVALID_CORE_STATE);
    unsafe { qpx_replica_free(h) };
    unsafe { qpx_replica_free(ptr::null_mut()) };
}

#[test]
fn bad_config_and_corrupt_records_are_rejected() {
    assert_eq!(replica_from(config(0, &[]), &[]).0, QPX_ERR_INVALID_CONFIG);
    let junk = qpx_bytes { ptr: b"junk".as_ptr(), len: 4 };
    let (rc, h) = replica_from(config(0, &[0]), &[junk]);
    assert_eq!(rc, QPX_ERR_CORRUPT_RECORD);
    assert!(h.is_null());
    let mut h = ptr::null_mut();
    assert_eq!(unsafe { qpx_replica_new(ptr::null(), ptr::null(), 0, 0, &mut h) }, QPX_ERR_INVALID_ARGUMENT);
    let mut cfg = ptr::null_mut();
    let seed = [0u8; 32];
    let rc = unsafe { qpx_config_new(ptr::null(), 0, ptr::null(), 0, seed.as_ptr(), &mut cfg) };
    assert_eq!(rc, QPX_ERR_INVALID_ARGUMENT, "a cluster id is required");
    unsafe { qpx_config_free(ptr::null_mut()) };
}

#[test]
fn status_strings_are_static_and_nul_terminated() {
    for rc in [QPX_OK, QPX_ERR_PANIC, QPX_ERR_POISONED, 12345] {
        let s = unsafe { std::ffi::CStr::from_ptr(qpx_status_str(rc)) };
        assert!(!s.to_bytes().is_empty());
    }
    assert_eq!(qpx_abi_version(), (QPX_ABI_VERSION_MAJOR << 16) | QPX_ABI_VERSION_MINOR);
}

fn last_error() -> String {
    let mut len = 0;
    assert_eq!(unsafe { qpx_last_error(ptr::null_mut(), 0, &mut len) }, QPX_BUFFER_TOO_SMALL);
    let mut buf = vec![0u8; len];
    assert_eq!(unsafe { qpx_last_error(buf.as_mut_ptr().cast(), len, &mut len) }, QPX_OK);
    std::ffi::CStr::from_bytes_with_nul(&buf).unwrap().to_string_lossy().into_owned()
}

#[test]
fn last_error_explains_an_invalid_config() {
    assert_eq!(replica_from(config(0, &[0, 0]), &[]).0, QPX_ERR_INVALID_CONFIG);
    assert!(last_error().contains("members must be unique"), "{}", last_error());
    // A later failure in argument checking replaces the old description.
    assert_eq!(unsafe { qpx_poll(ptr::null_mut(), ptr::null_mut()) }, QPX_ERR_INVALID_ARGUMENT);
    assert!(last_error().contains("invalid argument"), "{}", last_error());
}

#[test]
fn options_are_set_one_by_one_and_checked() {
    let cfg = config(0, &[0]);
    assert_eq!(unsafe { qpx_config_set(cfg, QPX_CONFIG_HEDGE_MAX_NS, 7_000_000) }, QPX_OK);
    assert_eq!(unsafe { qpx_config_set(cfg, QPX_CONFIG_MAX_IN_FLIGHT_SLOTS, 16) }, QPX_OK);
    // Too large for the option, or an option this library does not know: refused, named.
    assert_eq!(unsafe { qpx_config_set(cfg, QPX_CONFIG_MAX_BATCH_BYTES, 1 << 32) }, QPX_ERR_INVALID_ARGUMENT);
    assert!(last_error().contains("out of range"), "{}", last_error());
    assert_eq!(unsafe { qpx_config_set(cfg, 999, 1) }, QPX_ERR_INVALID_ARGUMENT);
    assert!(last_error().contains("unknown config option 999"), "{}", last_error());
    assert_eq!(
        unsafe { qpx_config_set(ptr::null_mut(), QPX_CONFIG_HEDGE_MIN_NS, 1) },
        QPX_ERR_INVALID_ARGUMENT
    );
    // Consistency is checked when the replica is created.
    assert_eq!(unsafe { qpx_config_set(cfg, QPX_CONFIG_MAX_IN_FLIGHT_SLOTS, 1000) }, QPX_OK);
    assert_eq!(replica_from(cfg, &[]).0, QPX_ERR_INVALID_CONFIG);
}

#[test]
fn options_take_effect_and_a_rejected_value_changes_nothing() {
    let cfg = config(0, &[0]);
    assert_eq!(unsafe { qpx_config_set(cfg, QPX_CONFIG_MAX_OUTSTANDING_PROPOSALS, 1) }, QPX_OK);
    let rejected =
        unsafe { qpx_config_set(cfg, QPX_CONFIG_MAX_OUTSTANDING_PROPOSALS, u64::from(u32::MAX) + 1) };
    assert_eq!(rejected, QPX_ERR_INVALID_ARGUMENT);
    let (rc, h) = replica_from(cfg, &[]);
    assert_eq!(rc, QPX_OK);
    let mut a = empty_action();
    while unsafe { qpx_poll(h, &mut a) } == QPX_OK {}
    let mut id = 0;
    assert_eq!(unsafe { qpx_propose(h, 0, b"x".as_ptr(), 1, &mut id) }, QPX_OK);
    while unsafe { qpx_poll(h, &mut a) } == QPX_OK {}
    assert_eq!(unsafe { qpx_propose(h, 0, b"y".as_ptr(), 1, &mut id) }, QPX_ERR_BUSY, "still at most one");
    unsafe { qpx_replica_free(h) };
    // The largest u32 is in range for every u32 option; every option is known.
    let cfg = config(0, &[0]);
    for option in [
        QPX_CONFIG_MAX_BATCH_BYTES,
        QPX_CONFIG_MAX_MESSAGE_BYTES,
        QPX_CONFIG_MAX_IN_FLIGHT_SLOTS,
        QPX_CONFIG_MAX_OUTSTANDING_PROPOSALS,
        QPX_CONFIG_RETRANSMIT_INTERVAL_NS,
        QPX_CONFIG_HEDGE_MIN_NS,
        QPX_CONFIG_HEDGE_MAX_NS,
    ] {
        assert_eq!(unsafe { qpx_config_set(cfg, option, u64::from(u32::MAX)) }, QPX_OK, "option {option}");
    }
    unsafe { qpx_config_free(cfg) };
}

#[test]
fn a_replica_keeps_its_own_copy_of_the_config() {
    let members = [0u32, 1, 2];
    let cfg = config(0, &members);
    let mut h = ptr::null_mut();
    assert_eq!(unsafe { qpx_replica_new(cfg, ptr::null(), 0, 0, &mut h) }, QPX_OK);
    unsafe { qpx_config_free(cfg) };
    let mut a = empty_action();
    while unsafe { qpx_poll(h, &mut a) } == QPX_OK {}
    let mut id = 0;
    assert_eq!(unsafe { qpx_propose(h, 0, b"x".as_ptr(), 1, &mut id) }, QPX_OK);
    unsafe { qpx_replica_free(h) };
}

#[test]
fn metrics_are_readable_over_the_abi() {
    let members = [0, 1, 2];
    let h = unsafe { new_replica(0, &members) };
    let garbage = b"not a message";
    let mut a = empty_action();
    while unsafe { qpx_poll(h, &mut a) } == QPX_OK {}
    assert_eq!(unsafe { qpx_handle_message(h, 0, garbage.as_ptr(), garbage.len()) }, QPX_OK);
    let mut v = 0;
    assert_eq!(unsafe { qpx_metric(h, QPX_METRIC_MESSAGES_REJECTED, &mut v) }, QPX_OK);
    assert_eq!(v, 1);
    assert_eq!(unsafe { qpx_metric(h, QPX_METRIC_LIVE_REGISTERS, &mut v) }, QPX_OK);
    for metric in 1..=QPX_METRIC_LIVE_REGISTERS {
        assert_eq!(unsafe { qpx_metric(h, metric, &mut v) }, QPX_OK, "metric {metric}");
    }
    assert_eq!(
        unsafe { qpx_metric(h, QPX_METRIC_LIVE_REGISTERS, ptr::null_mut()) },
        QPX_ERR_INVALID_ARGUMENT
    );
    assert_eq!(unsafe { qpx_metric(h, 0, &mut v) }, QPX_ERR_INVALID_ARGUMENT);
    assert!(last_error().contains("unknown metric 0"), "{}", last_error());
    unsafe { qpx_replica_free(h) };
}

#[test]
fn leader_order_is_a_permutation_of_the_members() {
    let members = [0u32, 1, 2];
    let with_order = |order: &[u32]| {
        let cfg = config(0, &members);
        assert_eq!(unsafe { qpx_config_set_leader_order(cfg, order.as_ptr(), order.len()) }, QPX_OK);
        let (rc, h) = replica_from(cfg, &[]);
        unsafe { qpx_replica_free(h) };
        rc
    };
    assert_eq!(with_order(&[2, 0, 1]), QPX_OK);
    assert_eq!(with_order(&[]), QPX_OK, "none: by id");
    assert_eq!(with_order(&[2, 2, 1]), QPX_ERR_INVALID_CONFIG);
    let cfg = config(0, &members);
    assert_eq!(unsafe { qpx_config_set_leader_order(cfg, ptr::null(), 3) }, QPX_ERR_INVALID_ARGUMENT);
    unsafe { qpx_config_free(cfg) };
}

#[test]
fn membership_changes_through_the_c_abi() {
    let members = [0, 1, 2];
    let r: Vec<_> = (0..N as u32).map(|i| unsafe { new_replica(i, &members) }).collect();
    let mut net = Vec::new();
    let mut seen = vec![Vec::new(); N];
    let run = |net: &mut Vec<(u32, Vec<u8>)>, seen: &mut Vec<Vec<Vec<u8>>>| {
        while !net.is_empty() {
            let (to, m) = net.remove(0);
            assert_eq!(unsafe { qpx_handle_message(r[to as usize], 0, m.as_ptr(), m.len()) }, QPX_OK);
            unsafe { drain(&r, to as usize, net, &mut seen[to as usize]) };
        }
    };
    for (i, c) in seen.iter_mut().enumerate() {
        unsafe { drain(&r, i, &mut net, c) };
    }
    // Replica 2 leaves; replica 1 becomes the preferred leader.
    let (next, order) = ([0u32, 1], [1u32, 0]);
    let mut id = 0;
    let rc = unsafe { qpx_propose_reconfiguration(r[0], 0, next.as_ptr(), 2, order.as_ptr(), 2, &mut id) };
    assert_eq!(rc, QPX_OK);
    unsafe { drain(&r, 0, &mut net, &mut seen[0]) };
    let again = unsafe { qpx_propose_reconfiguration(r[0], 0, members.as_ptr(), 3, ptr::null(), 0, &mut id) };
    assert_eq!(again, QPX_ERR_RECONFIGURATION_PENDING);
    let bad = unsafe { qpx_propose_reconfiguration(r[0], 0, ptr::null(), 3, ptr::null(), 0, &mut id) };
    assert_eq!(bad, QPX_ERR_INVALID_ARGUMENT);
    run(&mut net, &mut seen);
    for k in 0..70u32 {
        let p = k.to_le_bytes();
        assert_eq!(unsafe { qpx_propose(r[1], 0, p.as_ptr(), p.len(), &mut id) }, QPX_OK);
        unsafe { drain(&r, 1, &mut net, &mut seen[1]) };
        run(&mut net, &mut seen);
    }
    for (i, s) in seen.iter().enumerate() {
        let changes: Vec<String> = s
            .iter()
            .filter(|e| e.starts_with(b"epoch"))
            .map(|e| String::from_utf8_lossy(e).into_owned())
            .collect();
        assert_eq!(changes, ["epoch 1 from 64 [1, 0]"], "replica {i}");
    }
    // The removed replica learns on its timer that the new members have the old slots.
    for (i, s) in seen.iter_mut().enumerate() {
        assert_eq!(unsafe { qpx_handle_timeout(r[i], 1_000_000_000) }, QPX_OK);
        unsafe { drain(&r, i, &mut net, s) };
    }
    run(&mut net, &mut seen);
    for (i, s) in seen.iter().enumerate() {
        let retired = s.iter().filter(|e| e.starts_with(b"retired")).count();
        assert_eq!((i, retired), (i, usize::from(i == 2)));
    }
    assert!(seen[2].iter().any(|e| e == b"retired 1"));
    for h in r {
        unsafe { qpx_replica_free(h) };
    }
}

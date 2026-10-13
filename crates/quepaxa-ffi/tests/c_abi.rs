//! Exercises the C ABI from Rust (also run under Miri to catch undefined behaviour).

use std::ptr;

use quepaxa_ffi::*;

const N: usize = 3;

unsafe fn new_replica(i: u32, members: &[u32; N]) -> *mut qpx_replica {
    let mut cfg = std::mem::MaybeUninit::<qpx_config>::uninit();
    assert_eq!(unsafe { qpx_config_init(cfg.as_mut_ptr(), size_of::<qpx_config>()) }, QPX_OK);
    let mut cfg = unsafe { cfg.assume_init() };
    cfg.cluster_id = *b"rust-c-abi-tests";
    cfg.id = i;
    cfg.members = members.as_ptr();
    cfg.members_len = N;
    cfg.seed = [i as u8 + 1; 32];
    let mut h = ptr::null_mut();
    assert_eq!(unsafe { qpx_replica_new(&cfg, ptr::null(), 0, 0, &mut h) }, QPX_OK);
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
    let mut cfg = std::mem::MaybeUninit::<qpx_config>::uninit();
    assert_eq!(unsafe { qpx_config_init(cfg.as_mut_ptr(), size_of::<qpx_config>()) }, QPX_OK);
    let mut cfg = unsafe { cfg.assume_init() };
    let mut h = ptr::null_mut();
    assert_eq!(unsafe { qpx_replica_new(&cfg, ptr::null(), 0, 0, &mut h) }, QPX_ERR_INVALID_CONFIG);
    let members = [0u32];
    cfg.members = members.as_ptr();
    cfg.members_len = 1;
    let junk = qpx_bytes { ptr: b"junk".as_ptr(), len: 4 };
    assert_eq!(unsafe { qpx_replica_new(&cfg, &junk, 1, 0, &mut h) }, QPX_ERR_CORRUPT_RECORD);
    cfg.struct_size = 8;
    assert_eq!(unsafe { qpx_replica_new(&cfg, ptr::null(), 0, 0, &mut h) }, QPX_ERR_INVALID_ARGUMENT);
    assert!(h.is_null());
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
    let mut cfg = std::mem::MaybeUninit::<qpx_config>::uninit();
    assert_eq!(unsafe { qpx_config_init(cfg.as_mut_ptr(), size_of::<qpx_config>()) }, QPX_OK);
    let mut cfg = unsafe { cfg.assume_init() };
    let members = [0u32, 0];
    cfg.members = members.as_ptr();
    cfg.members_len = 2;
    let mut h = ptr::null_mut();
    assert_eq!(unsafe { qpx_replica_new(&cfg, ptr::null(), 0, 0, &mut h) }, QPX_ERR_INVALID_CONFIG);
    assert!(last_error().contains("members must be unique"), "{}", last_error());
}

#[test]
fn config_layouts_are_size_aware() {
    // Too small for any supported layout.
    let mut small = [0u8; 8];
    assert_eq!(unsafe { qpx_config_init(small.as_mut_ptr().cast(), small.len()) }, QPX_ERR_INVALID_ARGUMENT);
    // A caller compiled against a larger (future) layout: only our prefix is touched.
    #[repr(C)]
    struct Future {
        cfg: qpx_config,
        extra: u64,
    }
    let mut f = std::mem::MaybeUninit::<Future>::uninit();
    unsafe { (&raw mut (*f.as_mut_ptr()).extra).write(0xDEAD_BEEF) };
    assert_eq!(unsafe { qpx_config_init(f.as_mut_ptr().cast(), size_of::<Future>()) }, QPX_OK);
    let mut f = unsafe { f.assume_init() };
    assert_eq!((f.cfg.struct_size, f.extra), (size_of::<qpx_config>(), 0xDEAD_BEEF));
    let members = [0u32];
    f.cfg.members = members.as_ptr();
    f.cfg.members_len = 1;
    f.cfg.struct_size = size_of::<Future>();
    let mut h = ptr::null_mut();
    assert_eq!(unsafe { qpx_replica_new(&f.cfg, ptr::null(), 0, 0, &mut h) }, QPX_OK);
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
    let mut m = std::mem::MaybeUninit::<qpx_metrics>::zeroed();
    unsafe { (&raw mut (*m.as_mut_ptr()).struct_size).write(size_of::<qpx_metrics>()) };
    assert_eq!(unsafe { qpx_get_metrics(h, m.as_mut_ptr()) }, QPX_OK);
    let m = unsafe { m.assume_init() };
    assert_eq!(m.messages_rejected, 1);
    unsafe { qpx_replica_free(h) };
}

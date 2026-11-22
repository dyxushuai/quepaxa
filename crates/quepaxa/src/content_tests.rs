use super::*;
use alloc::collections::BTreeSet;
use alloc::vec;

fn id(origin: u32, seq: u64) -> DataId {
    DataId { origin: NodeId(origin), incarnation: 1, seq }
}

fn batch(len: usize, fill: u8) -> Bytes {
    Bytes::from(vec![fill; len])
}

fn reference(id: DataId, bytes: &Bytes) -> Ref {
    Ref { id, len: bytes.len() as u32, crc: crc32c(bytes) }
}

/// The loose byte counts agree with the batches, and each fits the cap.
fn check_accounting(s: &Store) {
    let mut loose: BTreeMap<NodeId, usize> = BTreeMap::new();
    for (id, h) in &s.batches {
        if !h.pinned(s.floor) {
            *loose.entry(id.origin).or_default() += h.bytes.len();
        }
    }
    assert_eq!(s.loose, loose);
    assert!(s.loose.values().all(|&n| n <= s.cap), "{:?} over {}", s.loose, s.cap);
}

#[test]
fn a_batch_is_held_once() {
    let mut s = Store::new(100);
    assert!(s.insert(id(1, 0), batch(10, 1)));
    assert!(s.insert(id(1, 0), batch(10, 2)));
    assert_eq!(s.get(&id(1, 0)), Some(&batch(10, 1)));
    assert_eq!(s.get(&id(1, 1)), None);
    check_accounting(&s);
}

#[test]
fn unpinned_batches_are_capped_per_origin_oldest_first() {
    let mut s = Store::new(25);
    for seq in 0..3 {
        assert!(s.insert(id(1, seq), batch(10, 0)));
    }
    // 30 bytes from origin 1 over a cap of 25: its oldest goes, another origin's stay.
    assert!(s.insert(id(2, 0), batch(20, 0)));
    assert_eq!(s.get(&id(1, 0)), None);
    assert!(s.get(&id(1, 1)).is_some() && s.get(&id(1, 2)).is_some() && s.get(&id(2, 0)).is_some());
    // Older than everything held: it is the one to go.
    assert!(!s.insert(id(1, 0), batch(10, 0)));
    check_accounting(&s);
}

#[test]
fn a_pin_checks_length_and_crc() {
    let mut s = Store::new(100);
    let bytes = batch(10, 7);
    s.insert(id(1, 0), bytes.clone());
    let good = reference(id(1, 0), &bytes);
    assert_eq!(s.pin(Ref { len: 9, ..good }, Slot(3)), Err(PinError::Mismatch));
    assert_eq!(s.pin(Ref { crc: good.crc ^ 1, ..good }, Slot(3)), Err(PinError::Mismatch));
    assert_eq!(s.pin(Ref { id: id(1, 1), ..good }, Slot(3)), Err(PinError::Missing));
    assert_eq!(s.pin(good, Slot(3)), Ok(()));
    check_accounting(&s);
}

// `Content.tla`'s seeded bug "evict content a register holds": a pinned batch outlives any
// pressure from its origin's later batches.
#[test]
fn a_pinned_batch_is_never_evicted_by_the_cap() {
    let mut s = Store::new(10);
    let bytes = batch(10, 1);
    s.insert(id(1, 0), bytes.clone());
    s.pin(reference(id(1, 0), &bytes), Slot(4)).unwrap();
    for seq in 1..5 {
        s.insert(id(1, seq), batch(10, 0));
    }
    assert_eq!(s.get(&id(1, 0)), Some(&bytes));
    assert_eq!(s.get(&id(1, 3)), None);
    assert!(s.get(&id(1, 4)).is_some());
    check_accounting(&s);
}

#[test]
fn a_slot_pin_lasts_until_compaction_passes_the_slot() {
    let mut s = Store::new(100);
    let (a, b) = (batch(10, 1), batch(10, 2));
    s.insert(id(1, 0), a.clone());
    s.insert(id(1, 1), b.clone());
    s.pin(reference(id(1, 0), &a), Slot(2)).unwrap();
    s.pin(reference(id(1, 0), &a), Slot(5)).unwrap();
    s.raise_floor(5);
    assert_eq!(s.get(&id(1, 0)), Some(&a), "slot 5 is above the floor");
    assert_eq!(s.referenced().map(|(i, _)| i).collect::<Vec<_>>(), [id(1, 0)]);
    s.raise_floor(6);
    assert_eq!(s.get(&id(1, 0)), None, "gone with its slots");
    assert_eq!(s.get(&id(1, 1)), Some(&b), "never referenced: loose, within the cap");
    assert_eq!(s.referenced().count(), 0);
    // Below the floor a slot pins nothing.
    s.pin(reference(id(1, 1), &b), Slot(3)).unwrap();
    assert_eq!(s.referenced().count(), 0);
    check_accounting(&s);
}

#[test]
fn a_kept_batch_stays_until_released() {
    let mut s = Store::new(10);
    let bytes = batch(10, 1);
    assert!(!s.keep(id(1, 0)));
    s.insert(id(1, 0), bytes.clone());
    assert!(s.keep(id(1, 0)));
    s.pin(reference(id(1, 0), &bytes), Slot(1)).unwrap();
    s.raise_floor(2);
    s.insert(id(1, 1), batch(10, 0));
    assert_eq!(s.get(&id(1, 0)), Some(&bytes), "kept past compaction and the cap");
    check_accounting(&s);
    s.release(id(1, 0));
    assert_eq!(s.get(&id(1, 0)), None, "loose again, the oldest goes");
    assert!(s.get(&id(1, 1)).is_some());
    check_accounting(&s);
}

#[test]
fn restored_batches_wait_for_their_records_before_the_cap() {
    let mut s = Store::new(10);
    let bytes: Vec<Bytes> = (0..3).map(|i| batch(10, i)).collect();
    for (seq, b) in bytes.iter().enumerate() {
        s.restore(id(1, seq as u64), b.clone());
    }
    s.pin(reference(id(1, 0), &bytes[0]), Slot(0)).unwrap();
    s.pin(reference(id(1, 1), &bytes[1]), Slot(1)).unwrap();
    s.raise_floor(0);
    assert!(s.get(&id(1, 0)).is_some() && s.get(&id(1, 1)).is_some());
    assert!(s.get(&id(1, 2)).is_some(), "10 loose bytes fit the cap");
    check_accounting(&s);
}

#[derive(Clone, Debug)]
enum Op {
    Insert(u32, u64, usize),
    Pin(u32, u64, u64),
    Keep(u32, u64),
    Release(u32, u64),
    Raise(u64),
}

fn arb_op() -> impl proptest::strategy::Strategy<Value = Op> {
    use proptest::prelude::*;
    prop_oneof![
        (0u32..2, 0u64..6, 1usize..12).prop_map(|(o, s, l)| Op::Insert(o, s, l)),
        (0u32..2, 0u64..6, 0u64..8).prop_map(|(o, s, t)| Op::Pin(o, s, t)),
        (0u32..2, 0u64..6).prop_map(|(o, s)| Op::Keep(o, s)),
        (0u32..2, 0u64..6).prop_map(|(o, s)| Op::Release(o, s)),
        (0u64..8).prop_map(Op::Raise),
    ]
}

proptest::proptest! {
    // Against a model of the pins: whatever a register, decision or keep pins is never
    // evicted, the byte accounting stays exact, and loose bytes stay within the cap.
    #[test]
    fn pinned_content_survives_any_sequence(ops in proptest::collection::vec(arb_op(), 1..60)) {
        let mut s = Store::new(20);
        let mut slots: BTreeMap<DataId, u64> = BTreeMap::new();
        let mut kept: BTreeSet<DataId> = BTreeSet::new();
        let mut floor = 0;
        for op in ops {
            match op {
                Op::Insert(o, seq, len) => {
                    s.insert(id(o, seq), batch(len, seq as u8));
                }
                Op::Pin(o, seq, slot) => {
                    let Some(b) = s.get(&id(o, seq)).cloned() else { continue };
                    s.pin(reference(id(o, seq), &b), Slot(slot)).unwrap();
                    if slot >= floor {
                        let t = slots.entry(id(o, seq)).or_default();
                        *t = (*t).max(slot);
                    }
                }
                Op::Keep(o, seq) => {
                    if s.keep(id(o, seq)) {
                        kept.insert(id(o, seq));
                    }
                }
                Op::Release(o, seq) => {
                    s.release(id(o, seq));
                    kept.remove(&id(o, seq));
                }
                Op::Raise(f) => {
                    floor = floor.max(f);
                    s.raise_floor(f);
                    slots.retain(|_, t| *t >= floor);
                }
            }
            for i in slots.keys().chain(&kept) {
                proptest::prop_assert!(s.get(i).is_some(), "{i:?} evicted while pinned");
            }
            check_accounting(&s);
        }
    }
}

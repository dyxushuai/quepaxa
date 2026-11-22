//! Data batches behind references (#172) before rules R and P: the formats are in place,
//! restart keeps content custody (promise 1), and references are refused.

use super::tests::{data_id, from_peer, proposal, replica_as};
use super::*;
use crate::batch::Ref;
use crate::crc::crc32c;

fn cfg() -> Config {
    Config::new([3; 16], NodeId(0), alloc::vec![NodeId(0), NodeId(1), NodeId(2)], [9; 32])
}

fn content() -> Bytes {
    let entry =
        Entry { origin: NodeId(1), incarnation: 1, id: ProposalId(0), payload: Bytes::from_static(b"x") };
    batch::encode(&[Item { entry, kind: Kind::App }])
}

/// A slot value referencing `content()` as data batch (1, 1, 0).
fn referencing() -> Proposal {
    let bytes = content();
    let r = Ref { id: data_id(1, 0), len: bytes.len() as u32, crc: crc32c(&bytes) };
    Proposal { value: batch::push_ref(&batch::encode(&[]), r), ..proposal(5, 1, 0) }
}

fn register(p: Proposal) -> Record {
    at_slot(0, p)
}

fn at_slot(slot: u64, p: Proposal) -> Record {
    Record::Recorder {
        slot: Slot(slot),
        reg: Register { step: 4, first: Some(p.clone()), agg: Some(p), prev_agg: None },
    }
}

fn data() -> Record {
    Record::Data { id: data_id(1, 0), bytes: content() }
}

fn start(records: &[Record]) -> Result<Replica, Error> {
    Replica::new(cfg(), records.iter().map(Record::encode), Timestamp(0))
}

#[test]
fn a_register_needs_its_content_on_disk_before_it() {
    assert!(start(&[Record::Incarnation(1), data(), register(referencing())]).is_ok());
    let missing = start(&[Record::Incarnation(1), register(referencing())]);
    assert_eq!(missing.err(), Some(Error::CorruptRecord { index: 1 }));
    let late = start(&[Record::Incarnation(1), register(referencing()), data()]);
    assert_eq!(late.err(), Some(Error::CorruptRecord { index: 1 }));
}

#[test]
fn a_register_whose_content_disagrees_is_corrupt() {
    let other = Record::Data { id: data_id(1, 0), bytes: Bytes::from_static(b"other bytes") };
    let r = start(&[Record::Incarnation(1), other, register(referencing())]);
    assert_eq!(r.err(), Some(Error::CorruptRecord { index: 2 }));
}

#[test]
fn a_decision_may_lack_its_content_but_never_commits_a_reference() {
    let decided = Record::Decided { slot: Slot(0), proposal: referencing() };
    let mut r = start(&[Record::Incarnation(1), decided]).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    // Refused, as every replica refuses it: the slot is applied with no entries.
    assert!(out.iter().any(|a| matches!(a, Action::Commit { slot: Slot(0), entries } if entries.is_empty())));
    assert_eq!(r.metrics().corrupt_batches, 1);
}

#[test]
fn a_checkpoint_rewrites_referenced_content_ahead_of_its_register() {
    let unreferenced = Record::Data { id: data_id(2, 9), bytes: content() };
    let decided = Record::Decided { slot: Slot(0), proposal: proposal(1, 0, 0) };
    let mut r =
        start(&[Record::Incarnation(1), unreferenced, data(), decided, at_slot(1, referencing())]).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    let state = r.core_state();
    r.compact(Timestamp(0), Slot(0), state).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    let Some(Action::Persist { records, replace: true, .. }) = out.last() else { panic!("{out:?}") };
    let records: Vec<Record> = records.iter().map(|b| Record::decode(b).unwrap()).collect();
    let at = |want: &Record| records.iter().position(|r| r == want);
    assert!(at(&data()).unwrap() < at(&at_slot(1, referencing())).unwrap(), "{records:?}");
    // Content nothing references is not rewritten.
    assert!(!records.iter().any(|r| matches!(r, Record::Data { id, .. } if *id == data_id(2, 9))));
}

#[test]
fn inbound_data_and_fetch_are_ignored() {
    let mut r = replica_as(0);
    for body in [
        Body::Data { id: data_id(1, 0), bytes: content() },
        Body::Fetch { slot: Slot(0), ids: alloc::vec![data_id(0, 0)] },
    ] {
        let before = r.metrics().messages_rejected;
        r.handle(Timestamp(1), from_peer(1, 0, body)).unwrap();
        let mut out = Vec::new();
        r.poll_actions(&mut out);
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(r.metrics().messages_rejected, before + 1);
        assert!(r.content.get(&data_id(1, 0)).is_none());
    }
}

#[test]
fn a_forward_carrying_a_reference_is_rejected() {
    let mut r = replica_as(0);
    let before = r.metrics().messages_rejected;
    r.handle(Timestamp(1), from_peer(1, 0, Body::Forward { batch: referencing().value })).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    assert_eq!(r.metrics().messages_rejected, before + 1);
    assert!(!out.iter().any(|a| matches!(a, Action::Send(_))), "no ack, nothing proposed: {out:?}");
    // The same entries inline are taken.
    r.handle(Timestamp(2), from_peer(1, 0, Body::Forward { batch: content() })).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.metrics().messages_rejected, before + 1);
    assert!(out.iter().any(|a| matches!(a, Action::Send(_) | Action::Persist { .. })), "{out:?}");
}

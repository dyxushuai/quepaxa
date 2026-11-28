//! Data batches behind references (#172): origin dissemination, rules R and P, fetch
//! rotation, held prefix for catch-up/retirement, restart custody, and `on_compacted` on
//! `emitted`.

use super::tests::{data_id, from_peer, proposal, replica_as};
use super::*;
use crate::batch::Ref;
use crate::crc::crc32c;

fn decode_msg(bytes: &Bytes) -> Message {
    let wire = WireCtx {
        cluster_id: [3; 16],
        config_epoch: 0,
        max_message_bytes: 16 << 20,
        max_value_bytes: 1 << 20,
    };
    Message::decode(bytes, &wire).unwrap_or_else(|_| {
        let alt = WireCtx { cluster_id: [1; 16], ..wire };
        Message::decode(bytes, &alt).unwrap()
    })
}

fn sent(out: &[Action]) -> Vec<Outbound> {
    out.iter()
        .flat_map(|a| match a {
            Action::Send(o) => alloc::vec![o.clone()],
            Action::Persist { then, .. } => then.clone(),
            _ => Vec::new(),
        })
        .collect()
}

fn drain_persists(r: &mut Replica, out: &mut Vec<Action>, now: Timestamp) -> Vec<Outbound> {
    let mut msgs = sent(out);
    let ids: Vec<PersistId> = out
        .iter()
        .filter_map(|a| match a {
            Action::Persist { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    out.clear();
    for id in ids {
        r.handle(now, Input::Persisted(id)).unwrap();
        let mut more = Vec::new();
        r.poll_actions(&mut more);
        msgs.extend(sent(&more));
    }
    msgs
}

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
fn a_decision_missing_content_fetches_and_emits_once_it_arrives() {
    let decided = Record::Decided { slot: Slot(0), proposal: referencing() };
    let mut r = start(&[Record::Incarnation(1), decided]).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    // Decision advances `committed`, while `emitted` and `held` wait for content.
    assert_eq!(r.commit_index(), 1);
    assert_eq!(r.emitted_index(), 0);
    assert_eq!(r.held, 0);
    assert!(!out.iter().any(|a| matches!(a, Action::Commit { .. })));
    // Once incarnation persists, the queued Fetch goes out to the origin (1).
    let mut msgs = drain_persists(&mut r, &mut out, Timestamp(1));
    assert!(
        msgs.iter().any(|o| {
            o.to == NodeId(1)
                && matches!(decode_msg(&o.bytes).body, Body::Fetch { slot: Slot(0), ref ids } if ids == &[data_id(1, 0)])
        }),
        "expected fetch to origin: {msgs:?}"
    );
    assert_eq!(r.metrics().fetches, 1);
    // Delivering the data writes Record::Data, advances held, and emits the commit.
    r.handle(Timestamp(2), from_peer(1, 0, Body::Data { id: data_id(1, 0), bytes: content() })).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    assert_eq!(r.emitted_index(), 1);
    assert_eq!(r.held, 1);
    assert!(out.iter().any(|a| matches!(a, Action::Commit { slot: Slot(0), entries } if entries.len() == 1 && entries[0].payload == Bytes::from_static(b"x"))));
    msgs.clear();
    let _ = drain_persists(&mut r, &mut out, Timestamp(3));
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
fn restart_pins_data_restored_after_decided_before_compaction() {
    let decided = Record::Decided { slot: Slot(1), proposal: referencing() };
    let slot0 = Record::Decided { slot: Slot(0), proposal: proposal(1, 0, 0) };
    let mut r = start(&[Record::Incarnation(1), slot0, decided, data()]).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    assert_eq!(r.emitted_index(), 2);
    // Compacting through slot 0 must keep slot 1's data in the checkpoint even though
    // `Record::Data` appeared after `Record::Decided` on disk.
    let state = CoreState {
        next: 1,
        leaders: cfg().leaders(),
        schedule: Schedule::with_order(cfg().leaders()),
        dedup: Dedup::default(),
    }
    .encode();
    r.compact(Timestamp(1), Slot(0), state).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    let Some(Action::Persist { records, replace: true, .. }) = out.last() else { panic!("{out:?}") };
    let records: Vec<Record> = records.iter().map(|b| Record::decode(b).unwrap()).collect();
    assert!(records.contains(&data()), "slot 1's data must be rewritten in checkpoint: {records:?}");
}

#[test]
fn origin_disseminates_large_chunks_as_data_and_forwards_a_ref() {
    let mut c = Config::new([3; 16], NodeId(1), alloc::vec![NodeId(0), NodeId(1), NodeId(2)], [9; 32]);
    c.inline_batch_bytes = 64;
    let mut r = Replica::new(c, [], Timestamp(0)).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    drain_persists(&mut r, &mut out, Timestamp(1));

    // A small payload stays inline.
    r.propose(Timestamp(2), Bytes::from_static(b"tiny")).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    let msgs = sent(&out);
    assert_eq!(r.metrics().data_sent, 0);
    assert!(msgs.iter().all(|o| !matches!(decode_msg(&o.bytes).body, Body::Data { .. })));

    // A large payload (> 64 bytes encoded) disseminates Data to all peers and sends a Ref in Forward.
    let big = Bytes::from(alloc::vec![b'a'; 128]);
    r.propose(Timestamp(3), big).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    let msgs = sent(&out);
    let data_to: Vec<NodeId> = msgs
        .iter()
        .filter(|o| matches!(decode_msg(&o.bytes).body, Body::Data { .. }))
        .map(|o| o.to)
        .collect();
    assert_eq!(data_to, alloc::vec![NodeId(0), NodeId(2)], "Data sent to every peer");
    assert_eq!(r.metrics().data_sent, 2);
    let fwd = msgs
        .iter()
        .find_map(|o| match decode_msg(&o.bytes).body {
            Body::Forward { batch, .. } if o.to == NodeId(0) => Some(batch),
            _ => None,
        })
        .expect("Forward to leader");
    assert_eq!(batch::refs(&fwd).len(), 1, "Forward carries a Ref");
}

#[test]
fn rule_r_parks_changing_requests_and_writes_data_before_recorder() {
    let mut r = replica_as(2);
    // Request that would change register at slot 0, without its Data: parked and fetched.
    let req = Body::RecordRequest { slot: Slot(0), step: 4, proposal: referencing() };
    r.handle(Timestamp(1), from_peer(0, 2, req)).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    assert_eq!(r.metrics().parked_requests, 1);
    assert!(r.registers.is_empty(), "parked request is not recorder state");
    let msgs = sent(&out);
    assert!(msgs.iter().any(|o| {
        o.to == NodeId(0)
            && matches!(decode_msg(&o.bytes).body, Body::Fetch { slot: Slot(0), ref ids } if ids == &[data_id(1, 0)])
    }));
    assert!(!msgs.iter().any(|o| matches!(decode_msg(&o.bytes).body, Body::RecordReply { .. })));

    // Delivering the Data replays on_request: Record::Data lands BEFORE Record::Recorder!
    r.handle(Timestamp(2), from_peer(0, 2, Body::Data { id: data_id(1, 0), bytes: content() })).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    let Some(Action::Persist { records, then, .. }) =
        out.iter().find(|a| matches!(a, Action::Persist { .. }))
    else {
        panic!("expected Persist: {out:?}");
    };
    let decoded: Vec<Record> = records.iter().map(|b| Record::decode(b).unwrap()).collect();
    let data_pos = decoded.iter().position(|r| matches!(r, Record::Data { id, .. } if *id == data_id(1, 0)));
    let rec_pos = decoded.iter().position(|r| matches!(r, Record::Recorder { slot: Slot(0), .. }));
    assert!(data_pos.unwrap() < rec_pos.unwrap(), "Data must precede Recorder: {decoded:?}");
    assert!(
        then.iter()
            .any(|o| o.to == NodeId(0) && matches!(decode_msg(&o.bytes).body, Body::RecordReply { .. })),
        "reply is gated behind Data + Recorder persist"
    );
}

#[test]
fn rule_r_answers_non_changing_requests_without_their_content() {
    let mut r = replica_as(2);
    // Record a higher-priority inline proposal at step 4 first.
    let high = proposal(10, 0, 0);
    r.handle(Timestamp(1), from_peer(0, 2, Body::RecordRequest { slot: Slot(0), step: 4, proposal: high }))
        .unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    drain_persists(&mut r, &mut out, Timestamp(2));

    // A lower-priority request at step 4 referencing missing Data would NOT change the register:
    // answered immediately without parking or fetching.
    let low = referencing(); // priority 5 < 10
    r.handle(Timestamp(3), from_peer(1, 2, Body::RecordRequest { slot: Slot(0), step: 4, proposal: low }))
        .unwrap();
    out.clear();
    r.poll_actions(&mut out);
    assert_eq!(r.metrics().parked_requests, 0);
    assert_eq!(r.metrics().fetches, 0);
    let msgs = sent(&out);
    assert!(
        msgs.iter()
            .any(|o| o.to == NodeId(1) && matches!(decode_msg(&o.bytes).body, Body::RecordReply { .. }))
    );
}

#[test]
fn rule_p_holds_proposer_requests_until_adopted_content_arrives() {
    let mut r = replica_as(0);
    // Start proposing at slot 0 with an inline entry.
    r.propose(Timestamp(1), Bytes::from_static(b"mine")).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    drain_persists(&mut r, &mut out, Timestamp(2));

    // Recorder 1 reports a higher step (step 5) with a referencing value that replica 0 lacks.
    let adopted = referencing();
    let reply = Body::RecordReply {
        slot: Slot(0),
        req_step: 4,
        step: 5,
        first: Some(crate::codec::WireProposal {
            priority: adopted.priority,
            key: adopted.key,
            value: Some(adopted.value.clone()),
        }),
        prev_agg: None,
    };
    r.handle(Timestamp(3), from_peer(1, 0, reply)).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    let msgs = sent(&out);
    // Must NOT send step-5 RecordRequests yet; must fetch from recorder 1 first.
    assert!(
        !msgs.iter().any(|o| matches!(decode_msg(&o.bytes).body, Body::RecordRequest { step: 5, .. })),
        "rule P must hold step-5 requests while content is missing"
    );
    assert!(
        msgs.iter().any(|o| {
            o.to == NodeId(1)
                && matches!(decode_msg(&o.bytes).body, Body::Fetch { slot: Slot(0), ref ids } if ids == &[data_id(1, 0)])
        }),
        "must fetch adopted content from reporting recorder 1"
    );

    // Once Data arrives, step-5 RecordRequests go out.
    r.handle(Timestamp(4), from_peer(1, 0, Body::Data { id: data_id(1, 0), bytes: content() })).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    let msgs = drain_persists(&mut r, &mut out, Timestamp(5));
    assert!(
        msgs.iter().any(|o| matches!(decode_msg(&o.bytes).body, Body::RecordRequest { step: 5, .. })),
        "step-5 requests sent once content arrives"
    );
}

#[test]
fn fetch_rotates_from_requester_to_origin_to_members() {
    let mut r = replica_as(2);
    let interval = r.cfg.retransmit_interval;
    // Peer 0 asks recorder 2 to record a proposal referencing origin 1's data (1, 1, 0).
    let req = Body::RecordRequest { slot: Slot(0), step: 4, proposal: referencing() };
    r.handle(Timestamp(1), from_peer(0, 2, req)).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    let fetch_targets = |out: &[Action]| -> Vec<NodeId> {
        sent(out)
            .into_iter()
            .filter(|o| matches!(decode_msg(&o.bytes).body, Body::Fetch { .. }))
            .map(|o| o.to)
            .collect()
    };
    assert_eq!(fetch_targets(&out), alloc::vec![NodeId(0)], "first attempt goes to requester 0");
    // Retry 1 goes to origin 1.
    r.handle_timeout(Timestamp(1 + interval)).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    assert_eq!(fetch_targets(&out), alloc::vec![NodeId(1)], "retry rotates to origin 1");
    assert_eq!(r.metrics().fetch_retries, 1);
    // Retry 2 wraps back to 0 after exponential backoff (2 * interval).
    r.handle_timeout(Timestamp(1 + 3 * interval)).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    assert_eq!(fetch_targets(&out), alloc::vec![NodeId(0)]);
    assert_eq!(r.metrics().fetch_retries, 2);
}

#[test]
fn blocked_on_content_asks_for_snapshot_when_holder_compacts_past_emitted() {
    let decided = Record::Decided { slot: Slot(0), proposal: referencing() };
    let mut r = start(&[Record::Incarnation(1), decided]).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    drain_persists(&mut r, &mut out, Timestamp(1));
    assert_eq!(r.commit_index(), 1);
    assert_eq!(r.emitted_index(), 0);
    // Peer 1 compacted through slot 0 (which is < committed, but >= emitted!).
    r.handle(Timestamp(2), from_peer(1, 0, Body::Compacted { through: Slot(0) })).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    assert!(
        out.iter().any(|a| matches!(a, Action::SnapshotNeeded { peer: NodeId(1), through: Slot(0) })),
        "on_compacted must guard on emitted, not committed: {out:?}"
    );
}

#[test]
fn catch_up_and_envelope_progress_use_held_prefix_not_committed() {
    // Replica 1 has decided slot 0 referencing missing data: committed=1, held=0.
    let mut c = Config::new([3; 16], NodeId(1), alloc::vec![NodeId(0), NodeId(1), NodeId(2)], [9; 32]);
    c.inline_batch_bytes = 64;
    let decided = Record::Decided { slot: Slot(0), proposal: referencing() };
    let mut r = Replica::new(c, [Record::Incarnation(1).encode(), decided.encode()], Timestamp(0)).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    drain_persists(&mut r, &mut out, Timestamp(1));
    assert_eq!(r.commit_index(), 1);
    assert_eq!(r.held, 0);

    // Peer 0 asks for CatchUp from slot 0 (as a removed replica probing progress for retirement).
    r.handle(Timestamp(2), from_peer(0, 1, Body::CatchUp { from: Slot(0) })).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    let msgs = drain_persists(&mut r, &mut out, Timestamp(3));
    let entries_msg = msgs
        .iter()
        .map(|o| decode_msg(&o.bytes))
        .find(|m| matches!(m.body, Body::Entries { .. }))
        .expect("Entries reply");
    assert_eq!(entries_msg.env.commit_index, 0, "envelope must report held (0), not committed (1)");
    assert!(
        matches!(entries_msg.body, Body::Entries { ref proposals, ref content, .. } if proposals.is_empty() && content.is_empty()),
        "cannot serve slots past held prefix"
    );

    // Once Data arrives, held becomes 1 and CatchUp serves slot 0 WITH its content.
    r.handle(Timestamp(4), from_peer(2, 1, Body::Data { id: data_id(1, 0), bytes: content() })).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    drain_persists(&mut r, &mut out, Timestamp(5));
    assert_eq!(r.held, 1);

    r.handle(Timestamp(6), from_peer(0, 1, Body::CatchUp { from: Slot(0) })).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    let msgs = drain_persists(&mut r, &mut out, Timestamp(7));
    let entries_msg = msgs
        .iter()
        .map(|o| decode_msg(&o.bytes))
        .find(|m| matches!(m.body, Body::Entries { .. }))
        .expect("Entries reply");
    assert_eq!(entries_msg.env.commit_index, 1, "envelope now reports held = 1");
    assert!(matches!(
        entries_msg.body,
        Body::Entries { ref proposals, ref content, .. } if proposals.len() == 1 && content.len() == 1
    ));
}

#[test]
fn mismatched_data_and_refs_are_rejected() {
    let mut r = replica_as(2);
    // Request parks and starts fetching (1, 1, 0) with expected len/crc.
    let req = Body::RecordRequest { slot: Slot(0), step: 4, proposal: referencing() };
    r.handle(Timestamp(1), from_peer(0, 2, req)).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    let before = r.metrics().messages_rejected;
    // Delivering wrong bytes under (1, 1, 0) is rejected and does not satisfy the fetch.
    r.handle(
        Timestamp(2),
        from_peer(0, 2, Body::Data { id: data_id(1, 0), bytes: Bytes::from_static(b"wrong") }),
    )
    .unwrap();
    out.clear();
    r.poll_actions(&mut out);
    assert_eq!(r.metrics().messages_rejected, before + 1);
    assert!(r.content.get(&data_id(1, 0)).is_none());
    // Inline_batch_bytes < 64 is rejected at config validation.
    let mut bad_cfg = cfg();
    bad_cfg.inline_batch_bytes = 63;
    assert!(matches!(Replica::new(bad_cfg, [], Timestamp(0)), Err(Error::InvalidConfig(_))));
}

#[test]
fn kept_refs_are_released_when_proposers_forwards_or_staged_items_drop() {
    // Tight store cap (128 bytes) so any released batch is evicted by newer loose data from
    // the same origin; a leaked `kept` flag would keep it pinned forever.
    let mut c = cfg();
    c.max_batch_bytes = 128;
    c.max_in_flight_slots = 1;
    let mut r = Replica::new(c, [], Timestamp(0)).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    drain_persists(&mut r, &mut out, Timestamp(1));

    let make_data = |seq: u64| {
        let e = Entry {
            origin: NodeId(1),
            incarnation: 1,
            id: ProposalId(seq),
            payload: Bytes::from(alloc::vec![seq as u8; 50]),
        };
        let bytes = batch::encode(&[Item { entry: e, kind: Kind::App }]);
        let id = data_id(1, seq);
        let rf = Ref { id, len: bytes.len() as u32, crc: crc32c(&bytes) };
        (id, bytes, rf)
    };
    let flood = |r: &mut Replica, from: u64| {
        for seq in from..from + 4 {
            r.content.insert(data_id(1, seq), Bytes::from(alloc::vec![0u8; 80]));
        }
    };

    // 1. Staged forward dropped on mismatch releases its kept sibling ref.
    let (id_a, bytes_a, ref_a) = make_data(0);
    let (id_b, _, ref_b) = make_data(1);
    r.content.insert(id_a, bytes_a);
    let bad_ref_b = Ref { crc: ref_b.crc ^ 1, ..ref_b };
    let staged = batch::encode_pieces(&[Piece::Ref(ref_a), Piece::Ref(bad_ref_b)]);
    r.handle(Timestamp(2), from_peer(1, 0, Body::Forward { seq: 0, batch: staged })).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    // Delivering bytes that mismatch `bad_ref_b` (without matching fetch check by inserting directly)
    // triggers `wake_pending_forwards` mismatch path and must release `id_a`.
    r.content.insert(id_b, Bytes::from(alloc::vec![9u8; ref_b.len as usize]));
    r.on_data(NodeId(1), data_id(2, 0), Bytes::from_static(b"wake"));
    flood(&mut r, 100);
    assert!(r.content.get(&id_a).is_none(), "sibling ref released when staged forward dropped");

    // 2. Proposer waiting on missing content dropped by `learn` releases its kept sibling ref.
    let (id_c, bytes_c, ref_c) = make_data(2);
    let (_, _, ref_d) = make_data(3);
    r.content.insert(id_c, bytes_c);
    r.propose(Timestamp(3), Bytes::from_static(b"x")).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    drain_persists(&mut r, &mut out, Timestamp(4));
    let adopted_val = batch::encode_pieces(&[Piece::Ref(ref_c), Piece::Ref(ref_d)]);
    let adopted = Proposal { value: adopted_val, ..proposal(9, 1, 9) };
    let reply = Body::RecordReply {
        slot: Slot(0),
        req_step: 4,
        step: 5,
        first: Some(crate::codec::WireProposal {
            priority: adopted.priority,
            key: adopted.key,
            value: Some(adopted.value.clone()),
        }),
        prev_agg: None,
    };
    r.handle(Timestamp(5), from_peer(1, 0, reply)).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    assert!(r.proposers[&Slot(0)].waiting_content);
    // Slot 0 decides with an inline value instead, removing the waiting proposer.
    r.learn(Slot(0), proposal(10, 2, 0), false, None);
    flood(&mut r, 200);
    assert!(r.content.get(&id_c).is_none(), "sibling ref released when waiting proposer decided elsewhere");

    // 3. `Piece::Ref` routed out by `forward_pending` releases its `keep` once handed on.
    let mut cf = Config::new([3; 16], NodeId(1), alloc::vec![NodeId(0), NodeId(1), NodeId(2)], [9; 32]);
    cf.max_batch_bytes = 128;
    cf.max_in_flight_slots = 1;
    let mut rf_node = Replica::new(cf, [], Timestamp(0)).unwrap();
    out.clear();
    rf_node.poll_actions(&mut out);
    drain_persists(&mut rf_node, &mut out, Timestamp(1));
    let (id_e, bytes_e, ref_e) = make_data(4);
    rf_node.content.insert(id_e, bytes_e);
    rf_node.content.keep(id_e);
    rf_node.entries.received(alloc::vec![Piece::Ref(ref_e)], true, 16, Timestamp(0));
    rf_node.drive();
    flood(&mut rf_node, 300);
    assert!(rf_node.content.get(&id_e).is_none(), "routed ref released after forward_pending");
}

#[test]
fn ref_forwards_track_ack_resend_compactly_and_ack_after_staging_wakes() {
    let mut fc = Config::new([3; 16], NodeId(1), alloc::vec![NodeId(0), NodeId(1), NodeId(2)], [9; 32]);
    fc.inline_batch_bytes = 64;
    let mut follower = Replica::new(fc, [], Timestamp(0)).unwrap();
    let mut leader = Replica::new(cfg(), [], Timestamp(0)).unwrap();
    let mut out = Vec::new();
    follower.poll_actions(&mut out);
    drain_persists(&mut follower, &mut out, Timestamp(1));
    leader.poll_actions(&mut out);
    drain_persists(&mut leader, &mut out, Timestamp(1));

    // Follower proposes a large payload: Data goes to peers, compact Ref Forward goes to leader.
    let id = follower.propose(Timestamp(2), Bytes::from(alloc::vec![b'z'; 128])).unwrap();
    out.clear();
    follower.poll_actions(&mut out);
    let msgs = sent(&out);
    let (data_id, data_bytes) = msgs
        .iter()
        .find_map(|o| match decode_msg(&o.bytes).body {
            Body::Data { id, bytes } if o.to == NodeId(0) => Some((id, bytes)),
            _ => None,
        })
        .expect("Data to leader");
    let (seq, fwd_batch) = msgs
        .iter()
        .find_map(|o| match decode_msg(&o.bytes).body {
            Body::Forward { seq, batch } if o.to == NodeId(0) => Some((seq, batch)),
            _ => None,
        })
        .expect("Forward to leader");
    assert!(!follower.watch.fwd.is_acked(id), "not acked before leader responds");

    // Unacknowledged past hedge_max: resend_forwards resends compact Ref (not inline 128 bytes).
    let due = follower.watch.fwd.due().expect("ref forward scheduled for resend");
    follower.handle_timeout(due).unwrap();
    out.clear();
    follower.poll_actions(&mut out);
    let resends: Vec<(u64, Bytes)> = sent(&out)
        .into_iter()
        .filter_map(|o| match decode_msg(&o.bytes).body {
            Body::Forward { seq, batch } if o.to == NodeId(0) => Some((seq, batch)),
            _ => None,
        })
        .collect();
    assert_eq!(resends.len(), 1);
    assert_eq!(resends[0].0, seq);
    assert_eq!(batch::refs(&resends[0].1).len(), 1, "resent forward stays a compact Ref");
    assert_eq!(follower.metrics().forward_resends, 1);

    // Leader receives Forward before Data: stages it and does NOT send ForwardAck yet.
    leader.handle(Timestamp(3), from_peer(1, 0, Body::Forward { seq, batch: fwd_batch })).unwrap();
    out.clear();
    leader.poll_actions(&mut out);
    assert!(
        !sent(&out).iter().any(|o| matches!(decode_msg(&o.bytes).body, Body::ForwardAck { .. })),
        "staged forward must not be acknowledged before Data arrives"
    );

    // Delivering Data wakes the staged forward and acknowledges `seq`.
    leader.handle(Timestamp(4), from_peer(1, 0, Body::Data { id: data_id, bytes: data_bytes })).unwrap();
    out.clear();
    leader.poll_actions(&mut out);
    let acks: Vec<u64> = sent(&out)
        .into_iter()
        .filter_map(|o| match decode_msg(&o.bytes).body {
            Body::ForwardAck { seqs } if o.to == NodeId(1) => Some(seqs),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(acks, alloc::vec![seq], "waking staged forward must acknowledge its seq");

    // Delivering ForwardAck marks the follower's entry acknowledged.
    follower.handle(Timestamp(5), from_peer(0, 1, Body::ForwardAck { seqs: acks })).unwrap();
    out.clear();
    follower.poll_actions(&mut out);
    assert!(follower.watch.fwd.is_acked(id), "ref-forwarded own entry marked acked");
}

#[test]
fn waking_missing_log_broadcasts_the_newly_held_tip() {
    let mut c = Config::new([3; 16], NodeId(1), alloc::vec![NodeId(0), NodeId(1), NodeId(2)], [9; 32]);
    c.inline_batch_bytes = 64;
    let p0 = Proposal {
        priority: crate::proposal::LEADER_PRIORITY,
        key: ValueKey { origin: NodeId(0), incarnation: 1, seq: 0 },
        value: Bytes::from_static(b"slot0"),
    };
    let data_bytes = content();
    let data_id = data_id(1, 0);
    let p1 = referencing();
    let records = [
        Record::Incarnation(1).encode(),
        Record::Decided { slot: Slot(0), proposal: p0 }.encode(),
        Record::Decided { slot: Slot(1), proposal: p1 }.encode(),
    ];
    let mut rep = Replica::new(c, records, Timestamp(0)).unwrap();
    let mut out = Vec::new();
    rep.poll_actions(&mut out);
    drain_persists(&mut rep, &mut out, Timestamp(1));
    assert_eq!(rep.commit_index(), 2);
    assert_eq!(rep.held, 1);

    // Delivering the missing data for slot 1 advances held to 2 and advertises slot 1 to peers.
    rep.handle(Timestamp(2), from_peer(2, 1, Body::Data { id: data_id, bytes: data_bytes })).unwrap();
    out.clear();
    rep.poll_actions(&mut out);
    assert_eq!(rep.held, 2);
    let decided: Vec<(NodeId, u64, Slot)> = sent(&out)
        .into_iter()
        .filter_map(|o| {
            let m = decode_msg(&o.bytes);
            match m.body {
                Body::Decided { slot, .. } => Some((o.to, m.env.commit_index, slot)),
                _ => None,
            }
        })
        .collect();
    assert_eq!(decided, [(NodeId(0), 2, Slot(1)), (NodeId(2), 2, Slot(1))]);
}

#[test]
fn lowering_in_flight_slots_at_runtime_does_not_evict_content_or_shrink_cap() {
    let mut c = cfg();
    c.max_batch_bytes = 256;
    c.max_in_flight_slots = 4;
    let mut r = Replica::new(c, [], Timestamp(0)).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    drain_persists(&mut r, &mut out, Timestamp(1));

    // Three 200-byte unpinned batches (600 B) fit in the 1024 B store cap, past a W=1 cap (256 B).
    for seq in 0..3 {
        assert!(r.content.insert(data_id(1, seq), Bytes::from(alloc::vec![seq as u8; 200])));
    }
    r.set_in_flight_slots(1).unwrap();
    assert_eq!(r.in_flight_slots(), 1);
    // A fourth 200-byte batch (800 B total <= 1024 B) still triggers zero evictions.
    assert!(r.content.insert(data_id(1, 3), Bytes::from(alloc::vec![3u8; 200])));
    for seq in 0..4 {
        assert!(r.content.get(&data_id(1, seq)).is_some(), "batch {seq} must not be evicted at W=1");
    }

    // Active driving respects the lowered W=1 even though the store cap stayed 1024 B.
    r.propose(Timestamp(2), Bytes::from(alloc::vec![b'a'; 180])).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    r.propose(Timestamp(3), Bytes::from(alloc::vec![b'b'; 180])).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    assert_eq!(r.proposers.len(), 1, "active driving must stay within W=1");

    // Sizing content_cache_bytes explicitly also decouples the store when max_in_flight_slots = 1.
    let mut c_static = cfg();
    c_static.max_batch_bytes = 256;
    c_static.max_in_flight_slots = 1;
    c_static.content_cache_bytes = Some(1024);
    let mut r_static = Replica::new(c_static, [], Timestamp(0)).unwrap();
    for seq in 0..4 {
        assert!(r_static.content.insert(data_id(1, seq), Bytes::from(alloc::vec![seq as u8; 200])));
    }
    for seq in 0..4 {
        assert!(
            r_static.content.get(&data_id(1, seq)).is_some(),
            "explicit content_cache_bytes holds batch {seq}"
        );
    }
}

#[test]
fn rule_r_parking_waits_for_arrival_grace_before_fetching() {
    let mut c = cfg();
    c.id = NodeId(2);
    c.retransmit_interval = 400_000_000;
    let mut rep = Replica::new(c, [], Timestamp(0)).unwrap();
    let mut out = Vec::new();
    rep.poll_actions(&mut out);
    drain_persists(&mut rep, &mut out, Timestamp(0));
    rep.watch.reply.insert(NodeId(0), 100_000_000);

    // Park a RecordRequest from the leader referencing missing Data from peer 1.
    let req = Body::RecordRequest { slot: Slot(0), step: 1, proposal: referencing() };
    rep.handle(Timestamp(1_000_000), from_peer(0, 2, req)).unwrap();
    out.clear();
    rep.poll_actions(&mut out);
    assert_eq!(rep.metrics().parked_requests, 1);
    assert_eq!(rep.metrics().fetches, 0, "no immediate Fetch while direct Data may still be in flight");

    // Once the arrival grace elapses without Data, the initial Fetch fires (not a retry).
    let due = rep.earliest_fetch().expect("grace timer scheduled");
    assert_eq!(due, Timestamp(401_000_000));
    rep.handle_timeout(due).unwrap();
    out.clear();
    rep.poll_actions(&mut out);
    assert_eq!(rep.metrics().fetches, 1);
    assert_eq!(rep.metrics().fetch_retries, 0);
    assert!(
        sent(&out)
            .iter()
            .any(|o| o.to == NodeId(0) && matches!(decode_msg(&o.bytes).body, Body::Fetch { .. })),
        "initial Fetch goes to requester after grace"
    );

    // Retries rotate peers and back off exponentially so a loaded bulk stream is not flooded.
    let retry1 = rep.earliest_fetch().expect("retry timer scheduled");
    assert_eq!(retry1, Timestamp(801_000_000));
    rep.handle_timeout(retry1).unwrap();
    out.clear();
    rep.poll_actions(&mut out);
    assert_eq!(rep.metrics().fetch_retries, 1);
    assert_eq!(rep.earliest_fetch(), Some(Timestamp(1_601_000_000)));
}

#[test]
fn staged_forward_defers_fetch_and_signals_liveness_without_acking_seq() {
    let mut c = cfg();
    c.retransmit_interval = 400_000_000;
    let mut leader = Replica::new(c, [], Timestamp(0)).unwrap();
    let mut out = Vec::new();
    leader.poll_actions(&mut out);
    drain_persists(&mut leader, &mut out, Timestamp(0));
    leader.commit_latency = Some(80_000_000);

    let data_bytes = content();
    let id = data_id(1, 0);
    let r = Ref { id, len: data_bytes.len() as u32, crc: crc32c(&data_bytes) };
    let fwd = Body::Forward { seq: 7, batch: batch::encode_pieces(&[Piece::Ref(r)]) };
    leader.handle(Timestamp(1_000_000), from_peer(1, 0, fwd)).unwrap();
    out.clear();
    leader.poll_actions(&mut out);

    // Leader proves liveness with an empty ForwardAck (seq 7 is NOT acked) and defers Fetch.
    assert_eq!(leader.metrics().fetches, 0, "Fetch deferred for in-flight Data");
    let acks: Vec<Vec<u64>> = sent(&out)
        .into_iter()
        .filter_map(|o| match decode_msg(&o.bytes).body {
            Body::ForwardAck { seqs } if o.to == NodeId(1) => Some(seqs),
            _ => None,
        })
        .collect();
    assert_eq!(acks, alloc::vec![Vec::<u64>::new()], "liveness ForwardAck does not ack staged seq");

    // Direct Data arriving within grace wakes the forward and acknowledges seq 7 with zero Fetches.
    leader.handle(Timestamp(10_000_000), from_peer(1, 0, Body::Data { id, bytes: data_bytes })).unwrap();
    out.clear();
    leader.poll_actions(&mut out);
    assert_eq!(leader.metrics().fetches, 0);
    leader.handle_timeout(Timestamp(100_000_000)).unwrap();
    out.clear();
    leader.poll_actions(&mut out);
    let acked_seqs: Vec<u64> = sent(&out)
        .into_iter()
        .filter_map(|o| match decode_msg(&o.bytes).body {
            Body::ForwardAck { seqs } if o.to == NodeId(1) => Some(seqs),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(acked_seqs, alloc::vec![7]);
}

#[test]
fn data_and_active_pipeline_lag_do_not_trigger_speculative_catch_up() {
    let cfg_with_id_and_w = || {
        let mut c = cfg();
        c.id = NodeId(2);
        c.max_in_flight_slots = 4;
        c.retransmit_interval = 400_000_000;
        c
    };
    let mut rep = Replica::new(cfg_with_id_and_w(), [], Timestamp(0)).unwrap();
    let mut out = Vec::new();
    rep.poll_actions(&mut out);
    drain_persists(&mut rep, &mut out, Timestamp(0));

    // A Body::Data arriving on the bulk stream advertising commit_index far ahead records the
    // peer's progress for timer-driven recovery without triggering an immediate CatchUp.
    let data_msg = Message {
        env: Envelope {
            cluster_id: [3; 16],
            config_epoch: 0,
            from: NodeId(0),
            to: NodeId(2),
            commit_index: 20,
        },
        body: Body::Data { id: data_id(0, 0), bytes: content() },
    };
    rep.handle(Timestamp(1_000_000), Input::Message(data_msg.encode())).unwrap();
    out.clear();
    rep.poll_actions(&mut out);
    assert_eq!(rep.metrics().messages_rejected, 0);
    assert_eq!(rep.catch_up.ahead(), 20, "Body::Data still records peer commit_index");
    assert!(
        !sent(&out).iter().any(|o| matches!(decode_msg(&o.bytes).body, Body::CatchUp { .. })),
        "Body::Data must not trigger immediate speculative CatchUp"
    );

    // On a fresh replica actively committing slot 0 at t=10 ms, hearing commit_index up to
    // committed + 2 * W (= 9) via a consensus message stays within the active pipeline window.
    let mut rep = Replica::new(cfg_with_id_and_w(), [], Timestamp(0)).unwrap();
    out.clear();
    rep.poll_actions(&mut out);
    drain_persists(&mut rep, &mut out, Timestamp(0));
    let p0 = Proposal {
        priority: 1,
        key: ValueKey { origin: NodeId(0), incarnation: 1, seq: 0 },
        value: Bytes::new(),
    };
    rep.handle(
        Timestamp(10_000_000),
        from_peer(
            0,
            2,
            Body::Entries { first: Slot(0), proposals: alloc::vec![p0.clone()], content: Vec::new() },
        ),
    )
    .unwrap();
    out.clear();
    rep.poll_actions(&mut out);
    drain_persists(&mut rep, &mut out, Timestamp(10_000_000));
    assert_eq!(rep.commit_index(), 1);

    let within = Message {
        env: Envelope {
            cluster_id: [3; 16],
            config_epoch: 0,
            from: NodeId(0),
            to: NodeId(2),
            commit_index: 9,
        },
        body: Body::Decided { slot: Slot(0), priority: p0.priority, key: p0.key },
    };
    rep.handle(Timestamp(20_000_000), Input::Message(within.encode())).unwrap();
    out.clear();
    rep.poll_actions(&mut out);
    assert_eq!(rep.metrics().messages_rejected, 0);
    assert!(
        !sent(&out).iter().any(|o| matches!(decode_msg(&o.bytes).body, Body::CatchUp { .. })),
        "active commits tolerate up to 2 * max_in_flight_slots of peer commit_index lead"
    );

    // Once quiet past retransmit_interval, CatchUp fires to recover the missed slots.
    rep.handle_timeout(Timestamp(420_000_000)).unwrap();
    out.clear();
    rep.poll_actions(&mut out);
    assert!(
        sent(&out).iter().any(|o| matches!(decode_msg(&o.bytes).body, Body::CatchUp { from: Slot(1) })),
        "stalled replica must request CatchUp after retransmit_interval"
    );
}

#[test]
fn learn_respects_scaled_fetch_grace_for_missing_refs() {
    let mut c = cfg();
    c.id = NodeId(2);
    c.retransmit_interval = 400_000_000;
    c.hedge_max = 3_000_000_000;
    let mut rep = Replica::new(c, [], Timestamp(0)).unwrap();
    let mut out = Vec::new();
    rep.poll_actions(&mut out);
    drain_persists(&mut rep, &mut out, Timestamp(0));
    rep.commit_latency = Some(300_000_000);
    rep.watch.reply.insert(NodeId(0), 250_000_000);

    // Learning a decided slot referencing missing Data defers Fetch by max(retransmit, 2 * lat) = 600 ms.
    let entries =
        Body::Entries { first: Slot(0), proposals: alloc::vec![referencing()], content: Vec::new() };
    rep.handle(Timestamp(1_000_000), from_peer(0, 2, entries)).unwrap();
    out.clear();
    rep.poll_actions(&mut out);
    assert_eq!(rep.metrics().fetches, 0, "learn must respect fetch_grace instead of fetching immediately");
    assert_eq!(rep.earliest_fetch(), Some(Timestamp(601_000_000)));
}

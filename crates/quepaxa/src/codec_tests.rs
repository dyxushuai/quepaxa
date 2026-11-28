use super::*;
use crate::proposal::tests::prop;

const CTX: WireCtx =
    WireCtx { cluster_id: [0xAB; 16], config_epoch: 7, max_message_bytes: 1 << 20, max_value_bytes: 1 << 19 };

fn env() -> Envelope {
    Envelope { cluster_id: [0xAB; 16], config_epoch: 7, from: NodeId(1), to: NodeId(2), commit_index: 9 }
}

fn hex(s: &str) -> Vec<u8> {
    let s: Vec<u8> = s.bytes().filter(u8::is_ascii_hexdigit).collect();
    s.chunks(2).map(|c| u8::from_str_radix(core::str::from_utf8(c).unwrap(), 16).unwrap()).collect()
}

const ENVELOPE_HEX: &str =
    "5150 02 KK abababababababababababababababab 0700000000000000 01000000 02000000 0900000000000000";

fn golden(kind: &str, body: &str) -> Vec<u8> {
    hex(&ENVELOPE_HEX.replace("KK", kind).replace(' ', "")).into_iter().chain(hex(body)).collect()
}

fn request() -> Message {
    let proposal = Proposal {
        priority: 0x0102,
        key: ValueKey { origin: NodeId(3), incarnation: 4, seq: 5 },
        value: Bytes::from_static(b"hi"),
    };
    Message { env: env(), body: Body::RecordRequest { slot: Slot(6), step: 4, proposal } }
}

#[test]
fn record_request_matches_golden_bytes_v2() {
    let body = "0600000000000000 04000000 0201000000000000 03000000 0400000000000000 0500000000000000 00 02000000 6869";
    assert_eq!(request().encode().to_vec(), golden("01", body));
}

#[test]
fn record_reply_matches_golden_bytes_v2() {
    let first = WireProposal {
        priority: 1,
        key: ValueKey { origin: NodeId(3), incarnation: 4, seq: 5 },
        value: None,
    };
    let msg = Message {
        env: env(),
        body: Body::RecordReply { slot: Slot(6), req_step: 4, step: 5, first: Some(first), prev_agg: None },
    };
    let body = "0600000000000000 04000000 05000000 02 0100000000000000 03000000 0400000000000000 0500000000000000 00";
    assert_eq!(msg.encode().to_vec(), golden("02", body));
}

#[test]
fn decided_matches_golden_bytes_v2_without_its_value() {
    let key = ValueKey { origin: NodeId(3), incarnation: 4, seq: 5 };
    let msg = Message { env: env(), body: Body::Decided { slot: Slot(6), priority: 0x0102, key } };
    let body = "0600000000000000 0201000000000000 03000000 0400000000000000 0500000000000000";
    let bytes = msg.encode();
    assert_eq!(bytes.to_vec(), golden("03", body));
    // Envelope 44 bytes, body 36: the leader's per-peer cost of a decision.
    assert_eq!(bytes.len(), 80);
    assert_eq!(Message::decode(&bytes, &CTX), Ok(msg));
}

const DATA_ID_HEX: &str = "03000000 0400000000000000 0500000000000000";

fn data_id() -> DataId {
    DataId { origin: NodeId(3), incarnation: 4, seq: 5 }
}

#[test]
fn data_and_fetch_match_golden_bytes_v2() {
    let data = Message { env: env(), body: Body::Data { id: data_id(), bytes: Bytes::from_static(b"hi") } };
    assert_eq!(data.encode().to_vec(), golden("0d", &[DATA_ID_HEX, "00 02000000 6869"].join(" ")));
    let fetch = Message { env: env(), body: Body::Fetch { slot: Slot(6), ids: alloc::vec![data_id()] } };
    let body = ["0600000000000000 01000000", DATA_ID_HEX].join(" ");
    assert_eq!(fetch.encode().to_vec(), golden("0e", &body));
}

#[test]
fn entries_match_golden_bytes_v2_with_their_content() {
    let proposal = Proposal {
        priority: 0x0102,
        key: ValueKey { origin: NodeId(3), incarnation: 4, seq: 5 },
        value: Bytes::from_static(b"v"),
    };
    let content = alloc::vec![(data_id(), Bytes::from_static(b"hi"))];
    let msg = Message {
        env: env(),
        body: Body::Entries { first: Slot(6), proposals: alloc::vec![proposal], content },
    };
    let body = [
        "0600000000000000 01000000",
        "0201000000000000 03000000 0400000000000000 0500000000000000 00 01000000 76",
        "01000000",
        DATA_ID_HEX,
        "00 02000000 6869",
    ]
    .join(" ");
    assert_eq!(msg.encode().to_vec(), golden("05", &body));
    assert_eq!(Message::decode(&msg.encode(), &CTX), Ok(msg));
}

#[test]
fn content_above_the_batch_limit_is_rejected() {
    let content = alloc::vec![(data_id(), Bytes::from_static(b"abc"))];
    let entries =
        Message { env: env(), body: Body::Entries { first: Slot(0), proposals: Vec::new(), content } };
    let data = Message { env: env(), body: Body::Data { id: data_id(), bytes: Bytes::from_static(b"abc") } };
    let tight = WireCtx { max_value_bytes: 2, ..CTX };
    for m in [entries, data] {
        assert_eq!(Message::decode(&m.encode(), &tight), Err(DecodeError::TooLarge));
        assert_eq!(Message::decode(&m.encode(), &CTX), Ok(m));
    }
}

#[test]
fn a_fetch_longer_than_its_bytes_is_rejected() {
    let fetch = Message { env: env(), body: Body::Fetch { slot: Slot(6), ids: alloc::vec![data_id()] } };
    let mut bytes = fetch.encode().to_vec();
    let n = bytes.len() - 24;
    bytes[n..n + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(Message::decode(&Bytes::from(bytes), &CTX), Err(DecodeError::Truncated));
}

#[test]
fn forward_ack_matches_golden_bytes_v2() {
    let ack = Message { env: env(), body: Body::ForwardAck { seqs: alloc::vec![7, 9] } };
    assert_eq!(ack.encode().to_vec(), golden("0a", "02000000 0700000000000000 0900000000000000"));
    assert_eq!(Message::decode(&ack.encode(), &CTX), Ok(ack));
}

#[test]
fn forward_matches_golden_bytes_v2() {
    let fwd = Message { env: env(), body: Body::Forward { seq: 7, batch: Bytes::from_static(b"ab") } };
    // The number, then the batch inline.
    let inline = alloc::format!("{:02x}", super::VALUE_INLINE);
    assert_eq!(
        fwd.encode().to_vec(),
        golden("06", &alloc::format!("0700000000000000 {inline} 02000000 6162"))
    );
    assert_eq!(Message::decode(&fwd.encode(), &CTX), Ok(fwd));
}

#[test]
fn rtt_messages_match_golden_bytes_v2() {
    let probe = Message {
        env: env(),
        body: Body::RttProbe {
            incarnation: 2,
            sent: 5,
            commit: 0x0304,
            table: alloc::vec![(NodeId(3), 0x0102)],
        },
    };
    let body = "0200000000000000 0500000000000000 0403000000000000 01000000 03000000 0201000000000000";
    assert_eq!(probe.encode().to_vec(), golden("0b", body));
    let reply = Message { env: env(), body: Body::RttReply { incarnation: 2, sent: 5 } };
    assert_eq!(reply.encode().to_vec(), golden("0c", "0200000000000000 0500000000000000"));
}

#[test]
fn an_rtt_table_longer_than_its_bytes_is_rejected() {
    let probe = Message {
        env: env(),
        body: Body::RttProbe { incarnation: 2, sent: 5, commit: 9, table: alloc::vec![(NodeId(3), 1)] },
    };
    let mut bytes = probe.encode().to_vec();
    let n = bytes.len() - 16;
    bytes[n..n + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(Message::decode(&Bytes::from(bytes), &CTX), Err(DecodeError::Truncated));
}

#[test]
fn read_query_and_reply_match_golden_bytes_v2() {
    let query = Message { env: env(), body: Body::ReadQuery { incarnation: 3, round: 7 } };
    assert_eq!(query.encode().to_vec(), golden("08", "0300000000000000 0700000000000000"));
    let reply =
        Message { env: env(), body: Body::ReadReply { incarnation: 3, round: 7, high: Slot(0x0102) } };
    assert_eq!(reply.encode().to_vec(), golden("09", "0300000000000000 0700000000000000 0201000000000000"));
}

#[test]
fn decode_shares_the_input_buffer() {
    let bytes = request().encode();
    let Body::RecordRequest { proposal, .. } = Message::decode(&bytes, &CTX).unwrap().body else { panic!() };
    assert_eq!(proposal.value.as_ptr(), bytes[bytes.len() - 2..].as_ptr());
}

#[test]
fn foreign_cluster_epoch_and_version_are_rejected() {
    let bytes = request().encode();
    let mut other = CTX;
    other.cluster_id = [0; 16];
    assert_eq!(Message::decode(&bytes, &other), Err(DecodeError::WrongCluster));
    let mut other = CTX;
    other.config_epoch = 8;
    assert_eq!(Message::decode(&bytes, &other), Err(DecodeError::WrongEpoch));
    let mut v = bytes.to_vec();
    v[2] = 1;
    assert_eq!(Message::decode(&Bytes::from(v), &CTX), Err(DecodeError::BadVersion));
}

#[test]
fn oversized_length_prefix_is_rejected_without_allocating() {
    let mut v = request().encode().to_vec();
    let n = v.len();
    v[n - 6..n - 2].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(Message::decode(&Bytes::from(v), &CTX), Err(DecodeError::Truncated));
}

#[test]
fn value_above_the_batch_limit_is_rejected() {
    let tight = WireCtx { max_value_bytes: 1, ..CTX };
    assert_eq!(Message::decode(&request().encode(), &tight), Err(DecodeError::TooLarge));
    let exact = WireCtx { max_value_bytes: 2, ..CTX };
    assert_eq!(Message::decode(&request().encode(), &exact), Ok(request()));
}

#[test]
fn message_above_limit_is_rejected() {
    let len = request().encode().len() as u32;
    let small = WireCtx { max_message_bytes: len - 1, ..CTX };
    assert_eq!(Message::decode(&request().encode(), &small), Err(DecodeError::TooLarge));
    let exact = WireCtx { max_message_bytes: len, ..CTX };
    assert_eq!(Message::decode(&request().encode(), &exact), Ok(request()));
}

#[test]
fn trailing_bytes_are_rejected() {
    let mut v = request().encode().to_vec();
    v.push(0);
    assert_eq!(Message::decode(&Bytes::from(v), &CTX), Err(DecodeError::Trailing));
}

#[test]
fn elided_value_resolves_only_against_the_same_key() {
    let mine = prop(9, 1, 0);
    let elided = reply_proposal(&mine, mine.key);
    assert_eq!(elided.value, None);
    assert_eq!(elided.clone().resolve(&mine), Some(mine.clone()));
    assert_eq!(elided.resolve(&prop(9, 2, 0)), None);
}

#[test]
fn other_proposals_keep_their_value_in_replies() {
    let other = prop(9, 2, 0);
    assert_eq!(reply_proposal(&other, prop(1, 1, 0).key).value, Some(other.value.clone()));
}

fn arb_prop() -> impl proptest::strategy::Strategy<Value = Proposal> {
    use proptest::prelude::*;
    (any::<u64>(), any::<u32>(), any::<u64>(), any::<u64>(), proptest::collection::vec(any::<u8>(), 0..64))
        .prop_map(|(priority, o, i, s, v)| Proposal {
            priority,
            key: ValueKey { origin: NodeId(o), incarnation: i, seq: s },
            value: Bytes::from(v),
        })
}

fn arb_wire() -> impl proptest::strategy::Strategy<Value = Option<WireProposal>> {
    use proptest::prelude::*;
    proptest::option::of((arb_prop(), any::<bool>()).prop_map(|(p, elide)| WireProposal {
        priority: p.priority,
        key: p.key,
        value: (!elide).then_some(p.value),
    }))
}

proptest::proptest! {
    #[test]
    fn request_round_trips(slot: u64, step: u32, from: u32, to: u32, ci: u64, p in arb_prop()) {
        let env = Envelope { from: NodeId(from), to: NodeId(to), commit_index: ci, ..env() };
        let m = Message { env, body: Body::RecordRequest { slot: Slot(slot), step, proposal: p.clone() } };
        let d = Message::decode(&m.encode(), &CTX).unwrap();
        let Body::RecordRequest { proposal, .. } = &d.body else { panic!() };
        proptest::prop_assert_eq!(&proposal.value, &p.value);
        proptest::prop_assert_eq!(d, m);
    }

    #[test]
    fn reply_round_trips(slot: u64, rs: u32, s: u32, f in arb_wire(), a in arb_wire()) {
        let m = Message { env: env(), body: Body::RecordReply { slot: Slot(slot), req_step: rs, step: s, first: f, prev_agg: a } };
        proptest::prop_assert_eq!(Message::decode(&m.encode(), &CTX).unwrap(), m);
    }
}

proptest::proptest! {
    #[test]
    fn catch_up_kinds_round_trip(from: u64, round: u64, ps in proptest::collection::vec(arb_prop(), 0..4)) {
        let batch = ps.first().map(|p| p.value.clone()).unwrap_or_default();
        for body in [
            Body::CatchUp { from: Slot(from) },
            Body::Decided { slot: Slot(from), priority: round, key: ValueKey { origin: NodeId(round as u32), incarnation: from, seq: round } },
            Body::Entries { first: Slot(from), proposals: ps.clone(), content: Vec::new() },
            Body::Forward { seq: round, batch },
            Body::Compacted { through: Slot(from) },
            Body::ReadQuery { incarnation: from, round },
            Body::ReadReply { incarnation: from, round, high: Slot(from) },
            Body::ForwardAck { seqs: alloc::vec![from, round] },
            Body::RttProbe { incarnation: 2, sent: from, commit: round, table: alloc::vec![(NodeId(round as u32), from), (NodeId(2), round)] },
            Body::RttReply { incarnation: 2, sent: round },
            Body::Data { id: DataId { origin: NodeId(round as u32), incarnation: from, seq: round }, bytes: ps.first().map(|p| p.value.clone()).unwrap_or_default() },
            Body::Fetch { slot: Slot(from), ids: ps.iter().map(|p| DataId { origin: p.key.origin, incarnation: p.key.incarnation, seq: p.key.seq }).collect() },
            Body::Entries { first: Slot(from), proposals: ps.clone(), content: ps.iter().map(|p| (DataId { origin: p.key.origin, incarnation: p.key.incarnation, seq: p.key.seq }, p.value.clone())).collect() },
        ] {
            let m = Message { env: env(), body };
            proptest::prop_assert_eq!(Message::decode(&m.encode(), &CTX).unwrap(), m);
        }
    }
}

#[test]
fn decode_never_panics_on_arbitrary_input() {
    bolero::check!().for_each(|input: &[u8]| {
        let _ = Message::decode(&Bytes::copy_from_slice(input), &CTX);
    });
}

#[test]
fn decode_never_panics_on_mutated_content_messages() {
    let content = alloc::vec![(data_id(), Bytes::from_static(b"hi")), (data_id(), Bytes::new())];
    let entries = Body::Entries { first: Slot(6), proposals: alloc::vec![prop(9, 1, 0)], content };
    let fetch = Body::Fetch { slot: Slot(6), ids: alloc::vec![data_id(), data_id()] };
    let bases: Vec<Vec<u8>> =
        [entries, fetch].into_iter().map(|body| Message { env: env(), body }.encode().to_vec()).collect();
    bolero::check!().with_type::<(bool, u16, u8, u8)>().for_each(|&(which, pos, val, cut)| {
        let mut v = bases[usize::from(which)].clone();
        let i = pos as usize % v.len();
        v[i] = val;
        v.truncate(v.len() - (cut as usize % 4));
        let _ = Message::decode(&Bytes::from(v), &CTX);
    });
}

#[test]
fn decode_never_panics_on_mutated_valid_messages() {
    let base = request().encode().to_vec();
    bolero::check!().with_type::<(u16, u8, u8)>().for_each(|&(pos, val, cut)| {
        let mut v = base.clone();
        let i = pos as usize % v.len();
        v[i] = val;
        v.truncate(v.len() - (cut as usize % 4));
        let _ = Message::decode(&Bytes::from(v), &CTX);
    });
}

#[test]
fn outbound_class_separates_control_consensus_and_bulk() {
    use crate::api::OutboundClass;
    let classify = |body| outbound_class(&Message { env: env(), body }.encode());
    assert_eq!(classify(Body::ForwardAck { seqs: Vec::new() }), OutboundClass::Control);
    assert_eq!(
        classify(Body::RttProbe { incarnation: 1, sent: 1, commit: 0, table: Vec::new() }),
        OutboundClass::Control
    );
    assert_eq!(classify(Body::RttReply { incarnation: 1, sent: 1 }), OutboundClass::Control);
    assert_eq!(classify(request().body), OutboundClass::Consensus);
    assert_eq!(classify(Body::Forward { seq: 1, batch: Bytes::new() }), OutboundClass::Consensus);
    assert_eq!(
        classify(Body::Decided {
            slot: Slot(1),
            priority: 1,
            key: ValueKey { origin: NodeId(1), incarnation: 1, seq: 1 }
        }),
        OutboundClass::Consensus
    );
    assert_eq!(classify(Body::Data { id: data_id(), bytes: Bytes::from_static(b"x") }), OutboundClass::Bulk);
    assert_eq!(
        classify(Body::Entries { first: Slot(0), proposals: Vec::new(), content: Vec::new() }),
        OutboundClass::Bulk
    );
    assert_eq!(outbound_class(b"bad"), OutboundClass::Consensus);
}

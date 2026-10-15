//! Replica unit tests that need crate-private wire and state types.

use super::*;
use crate::codec::reply_proposal;
use proptest::prelude::*;

fn replica() -> Replica {
    let cfg = Config::new([3; 16], NodeId(0), alloc::vec![NodeId(0), NodeId(1), NodeId(2)], [9; 32]);
    let mut r = Replica::new(cfg, [], Timestamp(0)).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    r.handle(Timestamp(0), Input::Persisted(PersistId(0))).unwrap();
    r.poll_actions(&mut out);
    r
}

fn proposal(priority: u64, origin: u32, seq: u64) -> Proposal {
    Proposal {
        priority,
        key: ValueKey { origin: NodeId(origin), incarnation: 1, seq },
        value: batch::encode(&[]),
    }
}

fn arb_body() -> impl Strategy<Value = Body> {
    let p = (any::<u64>(), 0u32..4, 0u64..3).prop_map(|(pr, o, s)| proposal(pr, o, s));
    let slot = prop_oneof![0u64..4, Just(u64::MAX), any::<u64>()].prop_map(Slot);
    let step = prop_oneof![0u32..12, Just(u32::MAX), any::<u32>()];
    prop_oneof![
        (slot.clone(), step.clone(), p.clone()).prop_map(|(slot, step, proposal)| Body::RecordRequest {
            slot,
            step,
            proposal
        }),
        (slot.clone(), step.clone(), step, proptest::option::of(p.clone()), proptest::option::of(p.clone()))
            .prop_map(|(slot, req_step, step, f, a)| {
                let key = ValueKey { origin: NodeId(0), incarnation: 1, seq: 0 };
                Body::RecordReply {
                    slot,
                    req_step,
                    step,
                    first: f.map(|f| reply_proposal(&f, key)),
                    prev_agg: a.map(|a| reply_proposal(&a, key)),
                }
            }),
        (slot.clone(), p.clone()).prop_map(|(slot, proposal)| Body::Decided { slot, proposal }),
        slot.clone().prop_map(|from| Body::CatchUp { from }),
        (slot.clone(), proptest::collection::vec(p, 0..3))
            .prop_map(|(first, proposals)| Body::Entries { first, proposals }),
        slot.prop_map(|through| Body::Compacted { through }),
    ]
}

#[test]
fn lost_catch_up_request_is_retried_on_the_timer() {
    let mut r = replica();
    let mut out = Vec::new();
    let catch_ups = |out: &[Action]| {
        out.iter()
            .filter(|a| match a {
                Action::Send(o) => {
                    matches!(
                        Message::decode(&o.bytes, &r_wire()),
                        Ok(Message { body: Body::CatchUp { .. }, .. })
                    )
                }
                _ => false,
            })
            .count()
    };
    // A peer advertises ten decided slots; the request it triggers is lost.
    let env =
        Envelope { cluster_id: [3; 16], config_epoch: 0, from: NodeId(1), to: NodeId(0), commit_index: 10 };
    let msg = Message { env, body: Body::CatchUp { from: Slot(0) } }.encode();
    r.handle(Timestamp(1), Input::Message(msg)).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(catch_ups(&out), 1);
    out.clear();
    let deadline = r.next_deadline().expect("a retry is scheduled");
    r.handle_timeout(deadline).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(catch_ups(&out), 1, "{out:?}");
    assert_eq!(r.metrics().catch_up_retries, 1);
}

#[test]
fn misaddressed_and_out_of_range_messages_are_rejected_at_the_exact_bounds() {
    let req = |slot: u64, step| Body::RecordRequest { slot: Slot(slot), step, proposal: proposal(1, 1, 0) };
    let reply =
        |req_step, step| Body::RecordReply { slot: Slot(0), req_step, step, first: None, prev_agg: None };
    // The documented limits, spelled out so a changed constant fails here.
    let (max_step, max_slot) = (u32::MAX - 16, u64::MAX - (1 << 32));
    let cases = [
        (1, 2, Body::CatchUp { from: Slot(0) }, true),
        (0, 0, Body::CatchUp { from: Slot(0) }, true),
        (7, 0, Body::CatchUp { from: Slot(0) }, true),
        (1, 0, Body::CatchUp { from: Slot(0) }, false),
        (1, 0, req(0, max_step + 1), true),
        (1, 0, req(0, max_step), false),
        (1, 0, req(max_slot + 1, 4), true),
        (1, 0, req(max_slot, 4), false),
        (1, 0, reply(5, 4), true),
        (1, 0, reply(4, max_step + 1), true),
        (1, 0, reply(4, max_step), false),
        (1, 0, reply(4, 4), false),
        (1, 0, Body::Forward { batch: Bytes::from_static(b"not a batch") }, true),
    ];
    for (from, to, body, rejected) in cases {
        let mut r = replica();
        let env = Envelope {
            cluster_id: [3; 16],
            config_epoch: 0,
            from: NodeId(from),
            to: NodeId(to),
            commit_index: 0,
        };
        let what = alloc::format!("{from}->{to} {body:?}");
        r.handle(Timestamp(1), Input::Message(Message { env, body }.encode())).unwrap();
        assert_eq!(r.metrics().messages_rejected, u64::from(rejected), "{what}");
    }
}

#[test]
fn a_decided_value_that_is_not_a_batch_commits_nothing_and_is_counted() {
    let mut r = replica();
    let env =
        Envelope { cluster_id: [3; 16], config_epoch: 0, from: NodeId(1), to: NodeId(0), commit_index: 0 };
    let proposal = Proposal { value: Bytes::from_static(b"garbage"), ..proposal(1, 1, 0) };
    let body = Body::Decided { slot: Slot(0), proposal };
    r.handle(Timestamp(1), Input::Message(Message { env, body }.encode())).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    assert!(out.iter().any(|a| matches!(a, Action::Commit { slot: Slot(0), entries } if entries.is_empty())));
    assert_eq!(r.metrics().corrupt_batches, 1);
}

#[test]
fn decisions_are_learned_up_to_the_last_valid_slot() {
    for (slot, learned) in [(0, 1), (MAX_SLOT, 1), (MAX_SLOT + 1, 0)] {
        let mut r = replica();
        let env = Envelope {
            cluster_id: [3; 16],
            config_epoch: 0,
            from: NodeId(1),
            to: NodeId(0),
            commit_index: 0,
        };
        let body = Body::Decided { slot: Slot(slot), proposal: proposal(1, 1, 0) };
        r.handle(Timestamp(1), Input::Message(Message { env, body }.encode())).unwrap();
        assert_eq!(r.metrics().slots_decided, learned, "slot {slot}");
    }
}

#[test]
fn snapshots_may_cover_slots_up_to_the_last_valid_one() {
    let members = [NodeId(0), NodeId(1), NodeId(2)];
    let state = |next| {
        CoreState {
            next,
            leaders: members.to_vec(),
            schedule: Schedule::new(&members),
            dedup: Dedup::default(),
        }
        .encode()
    };
    assert_eq!(replica().install_snapshot(Timestamp(1), Slot(MAX_SLOT - 1), state(MAX_SLOT)), Ok(true));
    assert_eq!(
        replica().install_snapshot(Timestamp(1), Slot(MAX_SLOT), state(MAX_SLOT + 1)),
        Err(Error::InvalidCoreState)
    );
}

#[test]
fn requests_below_the_floor_or_beyond_the_window_get_no_vote() {
    let members = [NodeId(0), NodeId(1), NodeId(2)];
    let state = CoreState {
        next: 10,
        leaders: members.to_vec(),
        schedule: Schedule::new(&members),
        dedup: Dedup::default(),
    }
    .encode();
    let replies = |slot: u64| {
        let mut r = replica();
        assert_eq!(r.install_snapshot(Timestamp(1), Slot(9), state.clone()), Ok(true));
        let mut out = Vec::new();
        r.poll_actions(&mut out);
        let env = Envelope {
            cluster_id: [3; 16],
            config_epoch: 0,
            from: NodeId(1),
            to: NodeId(0),
            commit_index: 0,
        };
        let body = Body::RecordRequest { slot: Slot(slot), step: 4, proposal: proposal(1, 1, 0) };
        r.handle(Timestamp(2), Input::Message(Message { env, body }.encode())).unwrap();
        out.clear();
        r.poll_actions(&mut out);
        let window = 4 * r.window();
        let bodies: Vec<Body> = out
            .iter()
            .flat_map(|a| match a {
                Action::Send(o) => alloc::vec![o.clone()],
                Action::Persist { then, .. } => then.clone(),
                _ => Vec::new(),
            })
            .filter_map(|o| Message::decode(&o.bytes, &r_wire()).ok().map(|m| m.body))
            .collect();
        (bodies, window)
    };
    let (below, window) = replies(3);
    assert!(matches!(below[..], [Body::Compacted { through: Slot(9) }]), "{below:?}");
    assert!(matches!(replies(10 + window - 1).0[..], [Body::RecordReply { .. }]));
    assert!(replies(10 + window).0.is_empty());
}

fn r_wire() -> WireCtx {
    WireCtx { cluster_id: [3; 16], config_epoch: 0, max_message_bytes: 16 << 20, max_value_bytes: 1 << 20 }
}

#[test]
fn decisions_are_applied_before_their_local_record_is_durable() {
    let mut r = replica();
    let mut out = Vec::new();
    // Ten decisions not originated by the leader (node 0) rotate it. They are applied at
    // once (a majority already holds each durably), and core state covers them.
    for slot in 0..10 {
        let env = Envelope {
            cluster_id: [3; 16],
            config_epoch: 0,
            from: NodeId(1),
            to: NodeId(0),
            commit_index: 0,
        };
        let body = Body::Decided { slot: Slot(slot), proposal: proposal(1, 1, slot) };
        r.handle(Timestamp(1), Input::Message(Message { env, body }.encode())).unwrap();
        r.poll_actions(&mut out);
    }
    assert_eq!(r.emitted_index(), 10);
    assert_eq!(out.iter().filter(|a| matches!(a, Action::Commit { .. })).count(), 10);
    let state = CoreState::decode(&r.core_state(), &r.cfg.members).unwrap();
    assert_eq!(state.schedule, r.schedule);
    assert_ne!(r.schedule, Schedule::new(&[NodeId(0), NodeId(1), NodeId(2)]));
}

proptest! {
    // Well-formed messages with adversarial field values from members never panic or
    // violate the API contract.
    #[test]
    fn arbitrary_member_messages_are_handled(msgs in proptest::collection::vec((1u32..3, arb_body()), 1..40)) {
        let mut r = replica();
        r.propose(Timestamp(1), Bytes::from_static(b"x")).unwrap();
        let mut out = Vec::new();
        r.poll_actions(&mut out);
        for (i, (from, body)) in msgs.into_iter().enumerate() {
            let env = Envelope { cluster_id: [3; 16], config_epoch: 0, from: NodeId(from), to: NodeId(0), commit_index: i as u64 };
            let bytes = Message { env, body }.encode();
            prop_assert!(r.handle(Timestamp(2 + i as u64), Input::Message(bytes)).is_ok());
            r.poll_actions(&mut out);
            let _ = r.handle_timeout(Timestamp(1_000_000_000 * (i as u64 + 1)));
            r.poll_actions(&mut out);
        }
    }
}

#[test]
fn one_local_change_at_a_time_even_before_it_is_decided() {
    let mut r = replica();
    let members = || alloc::vec![NodeId(0), NodeId(1), NodeId(2)];
    assert!(r.propose_reconfiguration(Timestamp(1), members(), Vec::new()).is_ok());
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    assert_eq!(
        r.propose_reconfiguration(Timestamp(1), members(), Vec::new()),
        Err(Error::ReconfigurationPending)
    );
}

#[test]
fn membership_changes_are_bounded_in_size() {
    let many = |n: usize| (0..n as u32).map(NodeId).collect::<Vec<_>>();
    let mut r = replica();
    let too_many =
        r.propose_reconfiguration(Timestamp(1), many(crate::schedule::MAX_MEMBERS + 1), Vec::new());
    assert!(matches!(too_many, Err(Error::InvalidConfig(_))));
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    assert!(r.propose_reconfiguration(Timestamp(1), many(crate::schedule::MAX_MEMBERS), Vec::new()).is_ok());
}

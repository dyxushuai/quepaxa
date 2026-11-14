//! Replica unit tests that need crate-private wire and state types.

use super::*;
use crate::codec::reply_proposal;
use crate::proposal::ValueKey;
use proptest::prelude::*;

fn replica() -> Replica {
    replica_as(0)
}

pub(super) fn replica_as(me: u32) -> Replica {
    let mut cfg = Config::new([3; 16], NodeId(me), alloc::vec![NodeId(0), NodeId(1), NodeId(2)], [9; 32]);
    cfg.silent_leader_takeover = true;
    let mut r = Replica::new(cfg, [], Timestamp(0)).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    r.handle(Timestamp(0), Input::Persisted(PersistId(0))).unwrap();
    r.poll_actions(&mut out);
    r
}

pub(super) fn proposal(priority: u64, origin: u32, seq: u64) -> Proposal {
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
        (any::<u64>(), proptest::collection::vec((0u32..4, any::<u64>()), 0..4)).prop_map(|(sent, rows)| {
            Body::RttProbe {
                incarnation: 1,
                sent,
                commit: sent / 3,
                table: rows.into_iter().map(|(n, rtt)| (NodeId(n), rtt)).collect(),
            }
        }),
        (0u64..3, any::<u64>()).prop_map(|(incarnation, sent)| Body::RttReply { incarnation, sent }),
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
        // A replica outside every known epoch: its requests are served, its replies and
        // decisions are not.
        (7, 0, Body::CatchUp { from: Slot(0) }, false),
        (7, 0, reply(4, 4), true),
        (7, 0, Body::Decided { slot: Slot(0), proposal: proposal(1, 7, 0) }, true),
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
fn restart_rejects_a_decision_beyond_the_last_valid_slot() {
    let cfg = || Config::new([3; 16], NodeId(0), alloc::vec![NodeId(0), NodeId(1), NodeId(2)], [9; 32]);
    let decided = |slot| Record::Decided { slot: Slot(slot), proposal: proposal(1, 1, 0) }.encode();
    assert!(Replica::new(cfg(), [decided(MAX_SLOT)], Timestamp(0)).is_ok());
    assert_eq!(
        Replica::new(cfg(), [decided(MAX_SLOT + 1)], Timestamp(0)).err(),
        Some(Error::CorruptRecord { index: 0 })
    );
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

pub(super) fn r_wire() -> WireCtx {
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
        r.cfg.placement = true;
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

#[test]
fn a_snapshot_is_asked_for_again_only_after_the_hedge_cap() {
    let mut r = replica();
    let hedge_max = r.cfg.hedge_max;
    let asks = |r: &mut Replica, at: u64| {
        let env = Envelope {
            cluster_id: [3; 16],
            config_epoch: 0,
            from: NodeId(1),
            to: NodeId(0),
            commit_index: 0,
        };
        let body = Body::Compacted { through: Slot(9) };
        r.handle(Timestamp(at), Input::Message(Message { env, body }.encode())).unwrap();
        let mut out = Vec::new();
        r.poll_actions(&mut out);
        out.iter().filter(|a| matches!(a, Action::SnapshotNeeded { through: Slot(9), .. })).count()
    };
    assert_eq!(asks(&mut r, 1), 1);
    assert_eq!(asks(&mut r, 2), 0, "the first ask may still be answered");
    assert_eq!(asks(&mut r, 1 + hedge_max), 1, "a lost snapshot is asked for again");
}

#[test]
fn catch_up_replies_wait_for_durability_and_are_sent_even_when_empty() {
    let mut r = replica();
    let mut out = Vec::new();
    let msg = |body| {
        let env = Envelope {
            cluster_id: [3; 16],
            config_epoch: 0,
            from: NodeId(1),
            to: NodeId(0),
            commit_index: 0,
        };
        Input::Message(Message { env, body }.encode())
    };
    let entries = |out: &[Action]| -> Vec<(u64, usize)> {
        out.iter()
            .flat_map(|a| match a {
                Action::Send(o) => alloc::vec![o],
                Action::Persist { then, .. } => then.iter().collect(),
                _ => Vec::new(),
            })
            .filter_map(|o| match Message::decode(&o.bytes, &r_wire()) {
                Ok(Message { env, body: Body::Entries { proposals, .. } }) => {
                    Some((env.commit_index, proposals.len()))
                }
                _ => None,
            })
            .collect()
    };
    // Nothing decided: the reply still goes out, reporting no progress.
    r.handle(Timestamp(1), msg(Body::CatchUp { from: Slot(0) })).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(entries(&out), [(0, 0)]);
    out.clear();
    // Slot 0 is decided but its record is not durable yet: a reply reporting it waits for
    // that write.
    r.handle(Timestamp(1), msg(Body::Decided { slot: Slot(0), proposal: proposal(1, 1, 0) })).unwrap();
    r.poll_actions(&mut out);
    let persist = out.iter().find_map(|a| match a {
        Action::Persist { id, .. } => Some(*id),
        _ => None,
    });
    out.clear();
    r.handle(Timestamp(1), msg(Body::CatchUp { from: Slot(0) })).unwrap();
    r.poll_actions(&mut out);
    assert!(entries(&out).is_empty(), "{out:?}");
    r.handle(Timestamp(1), Input::Persisted(persist.expect("the decision is written"))).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(entries(&out), [(1, 1)], "{out:?}");
}

#[test]
fn an_empty_catch_up_page_is_retried_only_on_the_timer() {
    let mut r = replica();
    let mut out = Vec::new();
    let from_peer = |commit_index, body| {
        let env =
            Envelope { cluster_id: [3; 16], config_epoch: 0, from: NodeId(1), to: NodeId(0), commit_index };
        Input::Message(Message { env, body }.encode())
    };
    // A peer advertises ten slots; the catch-up request comes back empty.
    r.handle(Timestamp(1), from_peer(10, Body::CatchUp { from: Slot(0) })).unwrap();
    r.poll_actions(&mut out);
    r.handle(Timestamp(2), from_peer(10, Body::Entries { first: Slot(0), proposals: Vec::new() })).unwrap();
    r.poll_actions(&mut out);
    let retry = r.next_deadline().expect("a retry is scheduled");
    assert!(retry >= Timestamp(2).after(r.cfg.retransmit_interval), "{retry:?}");
}

/// Replicas 0-2 are configured, 3 and 4 join, and the members change to `new`. The joiners
/// vote on the new epoch's slots but never learn a decision, so a slot the new epoch commits
/// (a retirement barrier among them) says nothing about who holds the old slots. Returns
/// the actions reported once the new epoch has committed past its first slot.
fn with_joiners_that_never_learn(new: &[u32]) -> Vec<(usize, Action)> {
    let members = alloc::vec![NodeId(0), NodeId(1), NodeId(2)];
    let mut rs: Vec<Replica> = (0..5u32)
        .map(|i| {
            let mut cfg = Config::new([5; 16], NodeId(i), members.clone(), [i as u8 + 1; 32]);
            // The widest window: recorders vote up to 256 slots past what they know, so the
            // joiners vote on the new epoch's first slots without holding the old ones.
            cfg.max_in_flight_slots = 64;
            Replica::new(cfg, [], Timestamp(0)).unwrap()
        })
        .collect();
    let wire = rs[0].wire;
    let learns = |b: &Body| matches!(b, Body::Decided { .. } | Body::Entries { .. } | Body::CatchUp { .. });
    let mut seen = Vec::new();
    let run = |rs: &mut Vec<Replica>, seen: &mut Vec<(usize, Action)>| {
        let mut queue = alloc::collections::VecDeque::new();
        loop {
            for (i, r) in rs.iter_mut().enumerate() {
                let mut out = Vec::new();
                r.poll_actions(&mut out);
                for a in out {
                    match a {
                        Action::Send(o) => queue.push_back((o.to.0 as usize, Input::Message(o.bytes))),
                        Action::Persist { id, then, .. } => {
                            queue
                                .extend(then.into_iter().map(|o| (o.to.0 as usize, Input::Message(o.bytes))));
                            queue.push_back((i, Input::Persisted(id)));
                        }
                        other => seen.push((i, other)),
                    }
                }
            }
            let Some((to, input)) = queue.pop_front() else { return };
            if to >= 3
                && let Input::Message(b) = &input
                && Message::decode(b, &wire).is_ok_and(|m| learns(&m.body))
            {
                continue; // the joiners never learn a decision
            }
            rs[to].handle(Timestamp(0), input).unwrap();
        }
    };
    run(&mut rs, &mut seen);
    rs[2]
        .propose_reconfiguration(Timestamp(0), new.iter().copied().map(NodeId).collect(), Vec::new())
        .unwrap();
    run(&mut rs, &mut seen);
    for k in 0..90u32 {
        rs[2].propose(Timestamp(0), Bytes::from(alloc::format!("w{k}"))).unwrap();
        run(&mut rs, &mut seen);
    }
    for t in 1..30u64 {
        for r in rs.iter_mut() {
            r.handle_timeout(Timestamp(t * 1_000_000_000)).unwrap();
        }
        run(&mut rs, &mut seen);
    }
    let from = seen
        .iter()
        .find_map(|(_, a)| match a {
            Action::Reconfigured { epoch: 1, from, .. } => Some(from.0),
            _ => None,
        })
        .expect("the change was applied");
    assert!(rs[2].commit_index() > from, "the new epoch commits with the joiners' votes");
    assert_eq!((rs[3].commit_index(), rs[4].commit_index()), (0, 0), "the joiners learned nothing");
    seen
}

#[test]
fn a_barrier_the_new_epoch_commits_does_not_retire_without_a_caught_up_majority() {
    let retired = |seen: Vec<(usize, Action)>| -> Vec<(usize, Action)> {
        seen.into_iter().filter(|(_, a)| matches!(a, Action::Retired { .. })).collect()
    };
    // Replicas 0 and 1 leave; of the new members only 2 holds the old slots.
    let r = retired(with_joiners_that_never_learn(&[2, 3, 4]));
    assert!(r.is_empty(), "retired with only one of three new members holding the old slots: {r:?}");
    // Replica 0 leaves; 1 and 2 hold the old slots, and half of four is not a majority.
    let r = retired(with_joiners_that_never_learn(&[1, 2, 3, 4]));
    assert!(r.is_empty(), "retired with only two of four new members holding the old slots: {r:?}");
}

#[test]
fn a_snapshot_resolving_the_retirement_barrier_still_reports_other_proposals() {
    // A snapshot that covers both an application proposal and the internal retirement
    // barrier reports the proposal and confirms (never reports) the barrier.
    let mut r = replica();
    let mut out = Vec::new();
    let app = r.propose(Timestamp(1), Bytes::from_static(b"x")).unwrap();
    let barrier = r.submit(Bytes::new(), Kind::Barrier).unwrap();
    r.retirement.submitted(1, barrier);
    r.poll_actions(&mut out);
    let mut dedup = Dedup::default();
    for id in [app, barrier] {
        dedup.first_time(NodeId(0), r.incarnation, id);
    }
    let members = [NodeId(0), NodeId(1), NodeId(2)];
    let state =
        CoreState { next: 10, leaders: members.to_vec(), schedule: Schedule::new(&members), dedup }.encode();
    assert_eq!(r.install_snapshot(Timestamp(2), Slot(9), state), Ok(true));
    out.clear();
    r.poll_actions(&mut out);
    assert!(out.iter().any(|a| matches!(a, Action::Committed { id, .. } if *id == app)), "{out:?}");
    assert!(!out.iter().any(|a| matches!(a, Action::ReadReady { id, .. } if *id == barrier)), "{out:?}");
    assert!(!matches!(r.retirement.phase(), retire::Phase::Draining { .. }), "the barrier was confirmed");
}

#[test]
fn a_catch_up_page_that_also_adds_the_replica_back_does_not_retire_it() {
    // One page carries the slot of the retirement barrier and, right after it, a change
    // adding the replica back: by the end of the call it is a member again.
    let mut r = replica();
    let mut out = Vec::new();
    let from_peer = |body| {
        let env = Envelope {
            cluster_id: [3; 16],
            config_epoch: 0,
            from: NodeId(1),
            to: NodeId(0),
            commit_index: 0,
        };
        Input::Message(Message { env, body }.encode())
    };
    let decided = |seq: u64, items: &[Item]| Proposal {
        priority: 1,
        key: ValueKey { origin: NodeId(1), incarnation: 1, seq },
        value: batch::encode(items),
    };
    let change = |id: u64, base: u64, members: &[u32]| Item {
        entry: Entry {
            origin: NodeId(1),
            incarnation: 1,
            id: ProposalId(id),
            payload: crate::schedule::encode_change(
                base,
                &members.iter().copied().map(NodeId).collect::<Vec<_>>(),
            ),
        },
        kind: Kind::Reconfigure,
    };
    // Slot 0 removes replica 0 from slot 64 on; slots 1-63 are empty.
    let mut first = alloc::vec![decided(0, &[change(0, 0, &[1, 2, 3])])];
    first.extend((1..64).map(|s| decided(s, &[])));
    r.handle(Timestamp(1), from_peer(Body::Entries { first: Slot(0), proposals: first })).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.schedule.epoch(), (1, 64), "removed from slot 64 on");
    let barrier = r.submit(Bytes::new(), Kind::Barrier).unwrap();
    r.retirement.submitted(1, barrier);
    let own = Item {
        entry: Entry { origin: NodeId(0), incarnation: r.incarnation, id: barrier, payload: Bytes::new() },
        kind: Kind::Barrier,
    };
    out.clear();
    let page = alloc::vec![decided(64, &[own]), decided(65, &[change(1, 1, &[0, 1, 2, 3])])];
    r.handle(Timestamp(2), from_peer(Body::Entries { first: Slot(64), proposals: page })).unwrap();
    r.poll_actions(&mut out);
    assert!(out.iter().any(|a| matches!(a, Action::Reconfigured { epoch: 2, .. })), "{out:?}");
    assert!(!out.iter().any(|a| matches!(a, Action::Retired { .. })), "retired although added back: {out:?}");
}

#[test]
fn a_change_learned_from_others_blocks_another_until_it_takes_effect() {
    let mut r = replica();
    let mut out = Vec::new();
    let order = crate::schedule::encode_change(0, &[NodeId(1), NodeId(2), NodeId(3)]);
    let change = Item {
        entry: Entry { origin: NodeId(1), incarnation: 1, id: ProposalId(0), payload: order },
        kind: Kind::Reconfigure,
    };
    let decided = Proposal {
        priority: 1,
        key: ValueKey { origin: NodeId(1), incarnation: 1, seq: 0 },
        value: batch::encode(&[change]),
    };
    let env =
        Envelope { cluster_id: [3; 16], config_epoch: 0, from: NodeId(1), to: NodeId(0), commit_index: 0 };
    let body = Body::Entries { first: Slot(0), proposals: alloc::vec![decided] };
    r.handle(Timestamp(1), Input::Message(Message { env, body }.encode())).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.schedule.epoch(), (1, 64));
    let again =
        r.propose_reconfiguration(Timestamp(1), alloc::vec![NodeId(0), NodeId(1), NodeId(2)], Vec::new());
    assert_eq!(again, Err(Error::ReconfigurationPending));
}

#[test]
fn a_local_change_overtaken_by_another_is_dropped_as_superseded() {
    let mut r = replica();
    let mut out = Vec::new();
    let mine =
        r.propose_reconfiguration(Timestamp(1), alloc::vec![NodeId(0), NodeId(1)], Vec::new()).unwrap();
    r.poll_actions(&mut out);
    let item = |origin: u32, incarnation: u64, id: ProposalId, members: &[u32]| Item {
        entry: Entry {
            origin: NodeId(origin),
            incarnation,
            id,
            payload: crate::schedule::encode_change(
                0,
                &members.iter().copied().map(NodeId).collect::<Vec<_>>(),
            ),
        },
        kind: Kind::Reconfigure,
    };
    let decided = |seq: u64, items: &[Item]| Proposal {
        priority: 1,
        key: ValueKey { origin: NodeId(1), incarnation: 1, seq },
        value: batch::encode(items),
    };
    // A peer's change from the same generation is decided first; then this replica's.
    let proposals = alloc::vec![
        decided(0, &[item(1, 1, ProposalId(0), &[1, 2])]),
        decided(1, &[item(0, r.incarnation, mine, &[0, 1])]),
    ];
    let env =
        Envelope { cluster_id: [3; 16], config_epoch: 0, from: NodeId(1), to: NodeId(0), commit_index: 0 };
    let body = Body::Entries { first: Slot(0), proposals };
    r.handle(Timestamp(2), Input::Message(Message { env, body }.encode())).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    assert_eq!(r.schedule.epoch(), (1, 64), "only the first change applies");
    assert_eq!(r.schedule.position(NodeId(0), Slot(64)), 2, "removed by the first change");
    assert!(out.contains(&Action::Dropped { id: mine, reason: DropReason::Superseded }), "{out:?}");
    assert!(!out.iter().any(|a| matches!(a, Action::Committed { id, .. } if *id == mine)), "{out:?}");
    assert_eq!(out.iter().filter(|a| matches!(a, Action::Reconfigured { .. })).count(), 1, "{out:?}");
}

#[test]
fn a_peers_superseded_change_does_not_drop_a_local_entry_with_the_same_id() {
    // Ids are per origin: a peer's change, overtaken by another, may share its id with an
    // application entry of this replica's own decided in the same batch.
    let mut r = replica();
    let mut out = Vec::new();
    let mine = r.propose(Timestamp(1), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    let inc = r.incarnation;
    let change = |origin: u32, id: ProposalId, members: &[u32]| Item {
        entry: Entry {
            origin: NodeId(origin),
            incarnation: inc,
            id,
            payload: crate::schedule::encode_change(
                0,
                &members.iter().copied().map(NodeId).collect::<Vec<_>>(),
            ),
        },
        kind: Kind::Reconfigure,
    };
    let own = Item {
        entry: Entry { origin: NodeId(0), incarnation: inc, id: mine, payload: Bytes::from_static(b"x") },
        kind: Kind::App,
    };
    let decided = |seq: u64, items: &[Item]| Proposal {
        priority: 1,
        key: ValueKey { origin: NodeId(1), incarnation: 1, seq },
        value: batch::encode(items),
    };
    let proposals = alloc::vec![
        decided(0, &[change(1, ProposalId(0), &[0, 1])]),
        decided(1, &[change(2, mine, &[0, 2]), own]),
    ];
    r.handle(Timestamp(2), from_peer(1, 0, Body::Entries { first: Slot(0), proposals })).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    assert_eq!(r.schedule.epoch(), (1, 64), "only the first change applies");
    assert!(out.contains(&Action::Committed { id: mine, slot: Slot(1) }), "{out:?}");
    assert!(!out.iter().any(|a| matches!(a, Action::Dropped { .. })), "{out:?}");
}

#[test]
fn a_local_change_decided_twice_in_one_batch_commits_once() {
    let mut r = replica();
    let mut out = Vec::new();
    let mine =
        r.propose_reconfiguration(Timestamp(1), alloc::vec![NodeId(0), NodeId(1)], Vec::new()).unwrap();
    r.poll_actions(&mut out);
    let copy = Item {
        entry: Entry {
            origin: NodeId(0),
            incarnation: r.incarnation,
            id: mine,
            payload: crate::schedule::encode_change(0, &[NodeId(0), NodeId(1)]),
        },
        kind: Kind::Reconfigure,
    };
    let decided = Proposal {
        priority: 1,
        key: ValueKey { origin: NodeId(1), incarnation: 1, seq: 0 },
        value: batch::encode(&[copy.clone(), copy]),
    };
    let env =
        Envelope { cluster_id: [3; 16], config_epoch: 0, from: NodeId(1), to: NodeId(0), commit_index: 0 };
    let body = Body::Entries { first: Slot(0), proposals: alloc::vec![decided] };
    r.handle(Timestamp(2), Input::Message(Message { env, body }.encode())).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    assert_eq!(r.schedule.epoch(), (1, 64));
    assert!(out.iter().any(|a| matches!(a, Action::Committed { id, .. } if *id == mine)), "{out:?}");
    assert!(!out.iter().any(|a| matches!(a, Action::Dropped { .. })), "{out:?}");
}

#[test]
fn a_local_change_resolved_by_a_snapshot_is_dropped_as_unresolved() {
    let mut r = replica();
    let mut out = Vec::new();
    let mine =
        r.propose_reconfiguration(Timestamp(1), alloc::vec![NodeId(0), NodeId(1)], Vec::new()).unwrap();
    r.poll_actions(&mut out);
    let mut dedup = Dedup::default();
    dedup.first_time(NodeId(0), r.incarnation, mine);
    let members = [NodeId(0), NodeId(1), NodeId(2)];
    let state =
        CoreState { next: 10, leaders: members.to_vec(), schedule: Schedule::new(&members), dedup }.encode();
    assert_eq!(r.install_snapshot(Timestamp(2), Slot(9), state), Ok(true));
    out.clear();
    r.poll_actions(&mut out);
    assert!(out.contains(&Action::Dropped { id: mine, reason: DropReason::Unresolved }), "{out:?}");
    assert!(!out.iter().any(|a| matches!(a, Action::Committed { id, .. } if *id == mine)), "{out:?}");
}

#[test]
fn a_snapshot_over_a_decided_but_unapplied_local_batch_releases_it() {
    // Slot 0 was started by a peer, so the local batch goes to slot 1; slot 1 is decided
    // with it while slot 0 is still unknown (nothing applied yet). A snapshot then covers
    // both: the batch, its entry committed, must not stay behind forever.
    let mut r = replica();
    let mut out = Vec::new();
    let from_peer = |body| {
        let env = Envelope {
            cluster_id: [3; 16],
            config_epoch: 0,
            from: NodeId(1),
            to: NodeId(0),
            commit_index: 0,
        };
        Input::Message(Message { env, body }.encode())
    };
    r.handle(
        Timestamp(1),
        from_peer(Body::RecordRequest { slot: Slot(0), step: 4, proposal: proposal(1, 1, 0) }),
    )
    .unwrap();
    r.poll_actions(&mut out);
    let id = r.propose(Timestamp(1), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    let (key, value, slot) =
        r.entries.batches().values().map(|b| (b.key, b.value.clone(), b.slot)).next().expect("a local batch");
    assert_eq!(slot, Some(Slot(1)));
    let decided = Proposal { priority: 1, key, value };
    r.handle(Timestamp(1), from_peer(Body::Decided { slot: Slot(1), proposal: decided })).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.emitted, 0, "slot 0 is still unknown");
    // Another local batch in flight in slot 2, the first one the snapshot leaves open.
    r.propose(Timestamp(1), Bytes::from_static(b"y")).unwrap();
    r.poll_actions(&mut out);
    let open = |r: &Replica| r.entries.batches().values().filter(|b| b.slot == Some(Slot(2))).count();
    assert_eq!(open(&r), 1);
    let mut dedup = Dedup::default();
    dedup.first_time(NodeId(0), r.incarnation, id);
    let members = [NodeId(0), NodeId(1), NodeId(2)];
    let state =
        CoreState { next: 2, leaders: members.to_vec(), schedule: Schedule::new(&members), dedup }.encode();
    assert_eq!(r.install_snapshot(Timestamp(2), Slot(1), state), Ok(true));
    r.poll_actions(&mut out);
    assert!(r.entries.batches().values().all(|b| b.key != key), "a covered, committed batch is released");
    assert_eq!(open(&r), 1, "a batch past the snapshot keeps its slot");
}

#[test]
fn a_follower_hands_a_hedged_batch_that_lost_its_slot_back_to_the_leader() {
    let mut r = replica_as(1);
    let mut out = Vec::new();
    let forwards = |out: &[Action]| {
        out.iter()
            .filter(|a| match a {
                Action::Send(o) => {
                    o.to == NodeId(0)
                        && matches!(
                            Message::decode(&o.bytes, &r_wire()),
                            Ok(Message { body: Body::Forward { .. }, .. })
                        )
                }
                _ => false,
            })
            .count()
    };
    let id = r.propose(Timestamp(1), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards(&out), 1);
    // The leader never commits it: the follower hedges, proposing it in a slot of its own.
    while r.entries.forwarded().contains_key(&id) {
        let at = r.next_deadline().expect("the hedge is scheduled");
        r.handle_timeout(at).unwrap();
    }
    r.poll_actions(&mut out);
    let slot = r.entries.batches().values().find_map(|b| b.slot).expect("the hedged batch has a slot");
    out.clear();
    // The leader's value takes the slot.
    let env =
        Envelope { cluster_id: [3; 16], config_epoch: 0, from: NodeId(0), to: NodeId(1), commit_index: 0 };
    let decided = Message { env, body: Body::Decided { slot, proposal: proposal(1, 0, 0) } }.encode();
    r.handle(r.now, Input::Message(decided)).unwrap();
    r.poll_actions(&mut out);
    assert!(r.entries.batches().is_empty(), "the follower does not compete with the leader again");
    assert_eq!(forwards(&out), 1, "the entry goes back to the leader");
    assert!(r.entries.forwarded().contains_key(&id), "and is hedged again if the leader stays silent");
}

#[test]
fn a_hedged_entry_does_not_stretch_the_hedge_delay() {
    const MS: u64 = 1_000_000;
    let mut r = replica_as(1);
    let mut out = Vec::new();
    r.commit_latency = Some(4 * MS);
    let delta = r.hedge_delay();
    let id = r.propose(Timestamp(1), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    // The leader stays silent: the entry is hedged and commits in this replica's own slot,
    // long after it was proposed.
    while r.entries.forwarded().contains_key(&id) {
        let at = r.next_deadline().expect("the hedge is scheduled");
        r.handle_timeout(at).unwrap();
        r.poll_actions(&mut out);
    }
    let (slot, key, value) = r
        .entries
        .batches()
        .values()
        .find_map(|b| Some((b.slot?, b.key, b.value.clone())))
        .expect("a hedged batch");
    let env =
        Envelope { cluster_id: [3; 16], config_epoch: 0, from: NodeId(2), to: NodeId(1), commit_index: 0 };
    let decided =
        Message { env, body: Body::Decided { slot, proposal: Proposal { priority: 1, key, value } } };
    r.handle(Timestamp(r.now.0 + 500 * MS), Input::Message(decided.encode())).unwrap();
    r.poll_actions(&mut out);
    assert!(out.iter().any(|a| matches!(a, Action::Committed { id: c, .. } if *c == id)), "{out:?}");
    // Its latency includes the hedge delay itself; learning from it would feed back.
    assert_eq!(r.hedge_delay(), delta);
}

pub(super) fn from_peer(from: u32, to: u32, body: Body) -> Input {
    let env = Envelope {
        cluster_id: [3; 16],
        config_epoch: 0,
        from: NodeId(from),
        to: NodeId(to),
        commit_index: 0,
    };
    Input::Message(Message { env, body }.encode())
}

/// The high-water mark `r` answers a read query with, sent at once (not gated on a persist).
fn mark(r: &mut Replica) -> u64 {
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    out.clear();
    r.handle(r.now, from_peer(1, 0, Body::ReadQuery { incarnation: 4, round: 9 })).unwrap();
    r.poll_actions(&mut out);
    let marks: Vec<u64> = out
        .iter()
        .filter_map(|a| match a {
            Action::Send(o) => match Message::decode(&o.bytes, &r_wire()).map(|m| m.body) {
                Ok(Body::ReadReply { incarnation: 4, round: 9, high }) => Some(high.0),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(marks.len(), 1, "one ungated reply: {out:?}");
    marks[0]
}

/// Persists every pending write and returns all records written so far.
fn persist_all(r: &mut Replica, records: &mut Vec<Bytes>) {
    let mut out = Vec::new();
    loop {
        r.poll_actions(&mut out);
        let persists: Vec<_> = out
            .drain(..)
            .filter_map(|a| match a {
                Action::Persist { id, records: rs, replace, .. } => Some((id, rs, replace)),
                _ => None,
            })
            .collect();
        if persists.is_empty() {
            return;
        }
        for (id, rs, replace) in persists {
            if replace {
                records.clear();
            }
            records.extend(rs);
            r.handle(r.now, Input::Persisted(id)).unwrap();
        }
    }
}

#[test]
fn a_recorded_slot_raises_the_mark_before_its_register_is_durable() {
    let mut r = replica();
    assert_eq!(mark(&mut r), 0);
    r.handle(
        Timestamp(1),
        from_peer(1, 0, Body::RecordRequest { slot: Slot(4), step: 4, proposal: proposal(1, 1, 0) }),
    )
    .unwrap();
    // The register write is still pending; the answer does not wait for it.
    assert_eq!(mark(&mut r), 5);
}

#[test]
fn the_mark_survives_register_removal_compaction_and_restart() {
    let mut r = replica();
    let mut records = Vec::new();
    persist_all(&mut r, &mut records);
    for slot in 0..3 {
        r.handle(
            Timestamp(1),
            from_peer(
                1,
                0,
                Body::RecordRequest { slot: Slot(slot), step: 4, proposal: proposal(1, 1, slot) },
            ),
        )
        .unwrap();
        persist_all(&mut r, &mut records);
        r.handle(
            Timestamp(1),
            from_peer(1, 0, Body::Decided { slot: Slot(slot), proposal: proposal(1, 1, slot) }),
        )
        .unwrap();
        persist_all(&mut r, &mut records);
    }
    assert!(r.registers.is_empty(), "registers go once their decisions are durable");
    assert_eq!(mark(&mut r), 3);
    // Restarted from what was durable.
    let cfg = Config::new([3; 16], NodeId(0), alloc::vec![NodeId(0), NodeId(1), NodeId(2)], [9; 32]);
    let mut restarted = Replica::new(cfg, records.clone(), Timestamp(2)).unwrap();
    // Nothing is sent before the new incarnation is durable.
    persist_all(&mut restarted, &mut Vec::new());
    assert_eq!(mark(&mut restarted), 3);
    // Compacted: the log below the floor is gone, the mark is not.
    let members = [NodeId(0), NodeId(1), NodeId(2)];
    let state = CoreState {
        next: 10,
        leaders: members.to_vec(),
        schedule: Schedule::new(&members),
        dedup: Dedup::default(),
    }
    .encode();
    assert_eq!(r.install_snapshot(Timestamp(3), Slot(9), state), Ok(true));
    persist_all(&mut r, &mut records);
    assert!(r.log.is_empty());
    assert_eq!(mark(&mut r), 10);
}

#[test]
fn anyone_may_ask_for_the_mark_but_only_members_answer_reads() {
    let mut r = replica();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    out.clear();
    r.handle(Timestamp(1), from_peer(7, 0, Body::ReadQuery { incarnation: 1, round: 1 })).unwrap();
    r.poll_actions(&mut out);
    assert!(matches!(&out[..], [Action::Send(o)] if o.to == NodeId(7)), "{out:?}");
    let rejected = r.metrics().messages_rejected;
    r.handle(Timestamp(1), from_peer(7, 0, Body::ReadReply { incarnation: 1, round: 1, high: Slot(5) }))
        .unwrap();
    assert_eq!(r.metrics().messages_rejected, rejected + 1);
}

/// The incarnation and round a read asked `peer` with, from what `r` sent.
fn read_round_sent(r: &mut Replica, peer: u32) -> (u64, u64) {
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    out.iter()
        .find_map(|a| match a {
            Action::Send(o) if o.to == NodeId(peer) => {
                match Message::decode(&o.bytes, &r_wire()).map(|m| m.body) {
                    Ok(Body::ReadQuery { incarnation, round }) => Some((incarnation, round)),
                    _ => None,
                }
            }
            _ => None,
        })
        .expect("a read query")
}

#[test]
fn an_answer_to_a_previous_incarnations_read_does_not_count() {
    let cfg = || Config::new([3; 16], NodeId(0), alloc::vec![NodeId(0), NodeId(1), NodeId(2)], [9; 32]);
    let mut records = Vec::new();
    let mut r = Replica::new(cfg(), [], Timestamp(0)).unwrap();
    persist_all(&mut r, &mut records);
    r.read_barrier(Timestamp(1)).unwrap();
    let old = read_round_sent(&mut r, 1);
    // Restarted, with slot 0 decided since: a fresh read needs a majority's answers.
    let mut r = Replica::new(cfg(), records.clone(), Timestamp(2)).unwrap();
    persist_all(&mut r, &mut Vec::new());
    r.handle(Timestamp(2), from_peer(1, 0, Body::Decided { slot: Slot(0), proposal: proposal(1, 1, 0) }))
        .unwrap();
    persist_all(&mut r, &mut Vec::new());
    let read = r.read_barrier(Timestamp(3)).unwrap();
    let new = read_round_sent(&mut r, 1);
    assert_eq!((new.0, new.1), (old.0 + 1, old.1), "the round number repeats; the incarnation does not");
    // A reply to the old round, delivered late, is not an answer to this one.
    r.handle(
        Timestamp(3),
        from_peer(1, 0, Body::ReadReply { incarnation: old.0, round: old.1, high: Slot(1) }),
    )
    .unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    assert!(!out.iter().any(|a| matches!(a, Action::ReadReady { id, .. } if *id == read)), "{out:?}");
}

#[test]
fn the_leader_leaves_the_next_leaders_slots_to_it() {
    // Slot 0 decides an order change that makes replica 1 the leader from slot 64; slots
    // 1-59 are empty, so this replica (the leader until then) drives slots up to 67.
    let mut r = replica();
    let mut out = Vec::new();
    let change = Item {
        entry: Entry {
            origin: NodeId(1),
            incarnation: 1,
            id: ProposalId(0),
            payload: crate::schedule::encode_change(0, &[NodeId(1), NodeId(0), NodeId(2)]),
        },
        kind: Kind::Reconfigure,
    };
    let decided = |seq: u64, items: &[Item]| Proposal {
        priority: 1,
        key: ValueKey { origin: NodeId(1), incarnation: 1, seq },
        value: batch::encode(items),
    };
    let mut proposals = alloc::vec![decided(0, &[change])];
    proposals.extend((1..60).map(|s| decided(s, &[])));
    r.handle(Timestamp(1), from_peer(1, 0, Body::Entries { first: Slot(0), proposals })).unwrap();
    r.poll_actions(&mut out);
    assert_eq!((r.committed, r.schedule.leader(Slot(64))), (60, NodeId(1)));
    for k in 0..20u32 {
        r.propose(Timestamp(2), Bytes::from(k.to_le_bytes().to_vec())).unwrap();
        r.poll_actions(&mut out);
    }
    let started: Vec<u64> = r.proposers.keys().map(|s| s.0).collect();
    assert_eq!(started, [60, 61, 62, 63], "only the slots this replica still leads");
    // Its slots decide elsewhere; past the change it hands its entries to the new leader.
    out.clear();
    let rest = (60..64).map(|s| decided(s, &[])).collect();
    r.handle(Timestamp(3), from_peer(1, 0, Body::Entries { first: Slot(60), proposals: rest })).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.committed, 64);
    let forwarded = out.iter().any(|a| {
        matches!(a, Action::Send(o) if o.to == NodeId(1)
            && matches!(Message::decode(&o.bytes, &r_wire()), Ok(Message { body: Body::Forward { .. }, .. })))
    });
    assert!(forwarded, "{out:?}");
    assert!(r.proposers.is_empty(), "nothing is proposed in the new leader's slots");
    // A follower that has not seen the change yet still hands this replica its entries.
    out.clear();
    let entry = Item {
        entry: Entry {
            origin: NodeId(2),
            incarnation: 1,
            id: ProposalId(7),
            payload: Bytes::from_static(b"late"),
        },
        kind: Kind::App,
    };
    let env =
        Envelope { cluster_id: [3; 16], config_epoch: 0, from: NodeId(2), to: NodeId(0), commit_index: 62 };
    let body = Body::Forward { batch: batch::encode(&[entry]) };
    r.handle(Timestamp(4), Input::Message(Message { env, body }.encode())).unwrap();
    r.poll_actions(&mut out);
    assert!(r.proposers.is_empty(), "a lagging follower's entries are not proposed here");
    let handed_on = out.iter().any(|a| {
        matches!(a, Action::Send(o) if o.to == NodeId(1)
            && matches!(Message::decode(&o.bytes, &r_wire()), Ok(Message { body: Body::Forward { .. }, .. })))
    });
    assert!(handed_on, "{out:?}");
}

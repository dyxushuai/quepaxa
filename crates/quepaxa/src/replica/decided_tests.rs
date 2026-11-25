//! `Decided` names a value by key: the receiver resolves it from what it holds, else catches
//! up from the sender, on the retransmit timer until it learns the slot.

use super::tests::{from_peer, proposal, r_wire, replica_as};
use super::*;
use crate::codec::reply_proposal;
use crate::proposal::LEADER_PRIORITY;

fn with_value(p: Proposal, value: &'static [u8]) -> Proposal {
    Proposal { value: Bytes::from_static(value), ..p }
}

fn sent(r: &mut Replica) -> Vec<(NodeId, Body)> {
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    let sends = out.into_iter().flat_map(|a| match a {
        Action::Send(o) => alloc::vec![o],
        Action::Persist { then, .. } => then,
        _ => Vec::new(),
    });
    sends.map(|o| (o.to, Message::decode(&o.bytes, &r_wire()).unwrap().body)).collect()
}

fn catch_ups(sent: &[(NodeId, Body)]) -> Vec<(NodeId, Slot)> {
    sent.iter()
        .filter_map(|(to, b)| match b {
            Body::CatchUp { from } => Some((*to, *from)),
            _ => None,
        })
        .collect()
}

fn decided_value(r: &Replica, slot: u64) -> Option<Bytes> {
    r.log.get(&Slot(slot)).map(|d| d.proposal.value.clone())
}

#[test]
fn a_decision_resolves_from_the_registers_first_value() {
    let mut r = replica_as(1);
    let p = with_value(proposal(LEADER_PRIORITY, 0, 0), b"first");
    r.handle(
        Timestamp(1),
        from_peer(0, 1, Body::RecordRequest { slot: Slot(0), step: 4, proposal: p.clone() }),
    )
    .unwrap();
    sent(&mut r);
    r.handle(Timestamp(2), from_peer(0, 1, Body::decided(Slot(0), p))).unwrap();
    assert!(catch_ups(&sent(&mut r)).is_empty());
    assert_eq!(decided_value(&r, 0).as_deref(), Some(&b"first"[..]));
    assert!(r.unresolved.is_empty());
}

#[test]
fn a_decision_resolves_from_the_registers_previous_aggregate() {
    let mut r = replica_as(1);
    let p = with_value(proposal(7, 0, 0), b"prev");
    let q = with_value(proposal(9, 2, 0), b"next");
    for (step, v) in [(4, &p), (5, &q)] {
        let body = Body::RecordRequest { slot: Slot(0), step, proposal: v.clone() };
        r.handle(Timestamp(1), from_peer(0, 1, body)).unwrap();
        sent(&mut r);
    }
    let reg = &r.registers[&Slot(0)];
    assert!(reg.first.as_ref().is_some_and(|f| f.key == q.key));
    assert!(reg.prev_agg.as_ref().is_some_and(|a| a.key == p.key));
    r.handle(Timestamp(2), from_peer(0, 1, Body::decided(Slot(0), p))).unwrap();
    assert!(catch_ups(&sent(&mut r)).is_empty());
    assert_eq!(decided_value(&r, 0).as_deref(), Some(&b"prev"[..]));
}

#[test]
fn a_decision_resolves_from_the_proposers_own_value() {
    let mut r = replica_as(0);
    r.propose(Timestamp(1), Bytes::from_static(b"mine")).unwrap();
    sent(&mut r);
    let own = r.proposers[&Slot(0)].prop.request_for(NodeId(1)).unwrap().clone();
    // Only the proposer holds it: no register, no local batch.
    r.registers.clear();
    r.entries.applied(own.key.seq);
    assert!(r.entries.batch_value(own.key).is_none());
    r.handle(Timestamp(2), from_peer(1, 0, Body::decided(Slot(0), own.clone()))).unwrap();
    assert!(catch_ups(&sent(&mut r)).is_empty());
    assert_eq!(decided_value(&r, 0), Some(own.value));
}

#[test]
fn a_decision_resolves_from_a_value_a_recorder_reported() {
    let mut r = replica_as(0);
    r.propose(Timestamp(1), Bytes::from_static(b"mine")).unwrap();
    sent(&mut r);
    let own = r.proposers[&Slot(0)].prop.request_for(NodeId(1)).unwrap().clone();
    // Recorder 1 reports another proposer's value first; only the proposer saw it.
    let other = with_value(proposal(3, 2, 0), b"theirs");
    let reply = Body::RecordReply {
        slot: Slot(0),
        req_step: 4,
        step: 4,
        first: Some(reply_proposal(&other, own.key)),
        prev_agg: None,
    };
    r.handle(Timestamp(2), from_peer(1, 0, reply)).unwrap();
    sent(&mut r);
    assert!(r.registers.get(&Slot(0)).is_none_or(|g| g.first.as_ref().is_none_or(|f| f.key != other.key)));
    r.handle(Timestamp(3), from_peer(1, 0, Body::decided(Slot(0), other))).unwrap();
    assert!(catch_ups(&sent(&mut r)).is_empty());
    assert_eq!(decided_value(&r, 0).as_deref(), Some(&b"theirs"[..]));
}

#[test]
fn an_unknown_value_is_caught_up_from_the_sender_and_retried_on_the_timer() {
    let mut r = replica_as(1);
    let p = with_value(proposal(1, 0, 0), b"unseen");
    r.handle(Timestamp(1), from_peer(0, 1, Body::decided(Slot(0), p.clone()))).unwrap();
    assert_eq!(catch_ups(&sent(&mut r)), [(NodeId(0), Slot(0))]);
    assert_eq!(r.metrics().slots_decided, 0);
    // A repeat announcement asks nothing more: one request per slot until the timer.
    r.handle(Timestamp(2), from_peer(0, 1, Body::decided(Slot(0), p.clone()))).unwrap();
    assert!(catch_ups(&sent(&mut r)).is_empty());
    let due = Timestamp(1).after(r.cfg.retransmit_interval);
    assert_eq!(r.next_deadline(), Some(due));
    r.handle_timeout(due).unwrap();
    // The decider may be gone: the retry goes to the slot's next member.
    assert_eq!(catch_ups(&sent(&mut r)), [(NodeId(2), Slot(0))]);
    let entries = Body::Entries { first: Slot(0), proposals: alloc::vec![p], content: Vec::new() };
    r.handle(due, from_peer(2, 1, entries)).unwrap();
    sent(&mut r);
    assert_eq!(decided_value(&r, 0).as_deref(), Some(&b"unseen"[..]));
    assert!(r.unresolved.is_empty(), "learning the slot clears its retry");
    let later = due.after(r.cfg.retransmit_interval);
    r.handle_timeout(later).unwrap();
    assert!(catch_ups(&sent(&mut r)).is_empty());
}

#[test]
fn unresolved_slots_share_one_catch_up_per_peer() {
    let mut r = replica_as(1);
    let mut asked = Vec::new();
    for slot in 0..3 {
        let p = proposal(1, 0, slot);
        r.handle(Timestamp(1), from_peer(0, 1, Body::decided(Slot(slot), p))).unwrap();
        asked.extend(catch_ups(&sent(&mut r)));
    }
    assert_eq!(asked, [(NodeId(0), Slot(0)), (NodeId(0), Slot(1)), (NodeId(0), Slot(2))]);
    r.handle_timeout(Timestamp(1).after(r.cfg.retransmit_interval)).unwrap();
    assert_eq!(catch_ups(&sent(&mut r)), [(NodeId(2), Slot(0))]);
}

/// Far ahead, a slot is not asked for at once (bulk catch-up is under way), but it is
/// remembered: bulk catch-up stops a slot short of the peers, so the timer asks for it.
#[test]
fn a_decision_far_ahead_is_asked_for_on_the_timer() {
    let mut r = replica_as(1);
    let far = Slot(4 * r.window());
    let p = with_value(proposal(1, 0, 9), b"far");
    r.handle(Timestamp(1), from_peer(0, 1, Body::decided(far, p.clone()))).unwrap();
    assert!(catch_ups(&sent(&mut r)).is_empty());
    let due = Timestamp(1).after(r.cfg.retransmit_interval);
    r.handle_timeout(due).unwrap();
    assert!(catch_ups(&sent(&mut r)).contains(&(NodeId(2), far)));
    r.handle(
        due,
        from_peer(2, 1, Body::Entries { first: far, proposals: alloc::vec![p], content: Vec::new() }),
    )
    .unwrap();
    sent(&mut r);
    assert_eq!(decided_value(&r, far.0).as_deref(), Some(&b"far"[..]));
    assert!(r.unresolved.is_empty());
}

#[test]
fn decided_broadcast_advertises_the_advanced_held_prefix() {
    let mut r0 = replica_as(0);
    let p0 = with_value(proposal(LEADER_PRIORITY, 0, 0), b"slot0");
    let p1 = with_value(proposal(LEADER_PRIORITY, 2, 1), b"slot1");
    // Replica 0 already learned slot 1 while a peer missed it; held is still 0.
    r0.learn(Slot(1), p1, false, Some(NodeId(2)));
    sent(&mut r0);
    assert_eq!(r0.held, 0);

    // Deciding slot 0 with broadcast advances held to 2 before encoding Decided,
    // so peers hear commit_index = 2 rather than the stale 0.
    r0.learn(Slot(0), p0.clone(), true, None);
    let mut out = Vec::new();
    r0.poll_actions(&mut out);
    let decided_to_n1 = out
        .into_iter()
        .find_map(|a| match a {
            Action::Send(o) if o.to == NodeId(1) => {
                let m = Message::decode(&o.bytes, &r_wire()).unwrap();
                matches!(m.body, Body::Decided { slot: Slot(0), .. }).then_some((o, m))
            }
            _ => None,
        })
        .expect("Decided broadcast for slot 0 to n1");
    assert_eq!(
        decided_to_n1.1.env.commit_index, 2,
        "Decided envelope must carry the post-learn held prefix (2), not stale 0"
    );

    // Feeding that message to n1 (which already knows slot 0's value from a record request)
    // commits slot 0 AND schedules slot 1 in `seen`, so n1 cannot stall behind in a quiet cluster.
    let mut r1 = replica_as(1);
    r1.handle(Timestamp(1), from_peer(0, 1, Body::RecordRequest { slot: Slot(0), step: 4, proposal: p0 }))
        .unwrap();
    sent(&mut r1);
    r1.handle(Timestamp(2), Input::Message(decided_to_n1.0.bytes)).unwrap();
    sent(&mut r1);
    assert_eq!(r1.commit_index(), 1);
    assert!(r1.seen.contains_key(&Slot(1)), "n1 must schedule slot 1 after hearing commit_index = 2");
    assert!(r1.next_deadline().is_some());
}

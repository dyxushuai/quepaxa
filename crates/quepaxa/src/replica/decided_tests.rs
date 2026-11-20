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
    let entries = Body::Entries { first: Slot(0), proposals: alloc::vec![p] };
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

/// Leader egress per slot with n = 3: two `RecordRequest`s of 89 + V bytes and two `Decided`
/// of 80, so (n − 1)(169 + V); the value used to travel in `Decided` too, (n − 1)(174 + 2V).
#[test]
fn leader_egress_per_slot_carries_the_value_once_per_peer() {
    let mut r = replica_as(0);
    r.propose(Timestamp(1), Bytes::from_static(b"a write")).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    let mut bytes = 0;
    let mut v = 0;
    let count = |out: &[Action], bytes: &mut usize, v: &mut usize| {
        for a in out {
            let sends = match a {
                Action::Send(o) => core::slice::from_ref(o),
                Action::Persist { then, .. } => &then[..],
                _ => &[],
            };
            for o in sends.iter().filter(|o| o.to != NodeId(0)) {
                match Message::decode(&o.bytes, &r_wire()).unwrap().body {
                    Body::RecordRequest { slot: Slot(0), proposal, .. } => {
                        *v = proposal.value.len();
                        *bytes += o.bytes.len();
                    }
                    Body::Decided { slot: Slot(0), .. } => *bytes += o.bytes.len(),
                    _ => {}
                }
            }
        }
    };
    count(&out, &mut bytes, &mut v);
    let persists: Vec<PersistId> = out
        .iter()
        .filter_map(|a| match a {
            Action::Persist { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    out.clear();
    for id in persists {
        r.handle(Timestamp(2), Input::Persisted(id)).unwrap();
    }
    let own = r.proposers[&Slot(0)].prop.request_for(NodeId(1)).unwrap().clone();
    let reply = Body::RecordReply {
        slot: Slot(0),
        req_step: 4,
        step: 4,
        first: Some(reply_proposal(&own, own.key)),
        prev_agg: None,
    };
    r.poll_actions(&mut out);
    count(&out, &mut bytes, &mut v);
    out.clear();
    r.handle(Timestamp(3), from_peer(1, 0, reply)).unwrap();
    r.poll_actions(&mut out);
    count(&out, &mut bytes, &mut v);
    assert!(r.log.contains_key(&Slot(0)), "decided on the fast path");
    assert!(v > 0);
    assert_eq!(bytes, 2 * (169 + v));
}

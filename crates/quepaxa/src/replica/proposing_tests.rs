//! Proposer side: reply counters, the leader's forward cap, split forwards, and a lost
//! slot's entries.

use super::tests::{from_peer, proposal, replica_as};
use super::watch_tests::forwards_to;
use super::*;
use crate::codec::reply_proposal;
use crate::proposal::LEADER_PRIORITY;

fn item(origin: u32, id: u64, payload: Bytes) -> Item {
    Item {
        entry: Entry { origin: NodeId(origin), incarnation: 1, id: ProposalId(id), payload },
        kind: Kind::App,
    }
}

/// Leader 0 driving the slot of one local entry, with the proposal it asks recorder 1 for.
fn leading() -> (Replica, Slot, Proposal) {
    let mut r = replica_as(0);
    let mut out = Vec::new();
    r.propose(Timestamp(1), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    let (&slot, a) = r.proposers.iter().next().expect("the leader drives a slot");
    let request = a.prop.request_for(NodeId(1)).cloned().expect("recorder 1 is asked");
    (r, slot, request)
}

#[test]
fn a_reply_whose_value_does_not_resolve_is_rejected() {
    let (mut r, slot, _) = leading();
    let step = r.proposers[&slot].prop.step();
    // A value sent by reference to a key the request did not carry.
    let foreign = proposal(1, 2, 0);
    let first = Some(reply_proposal(&foreign, foreign.key));
    r.handle(r.now, from_peer(1, 0, Body::RecordReply { slot, req_step: step, step, first, prev_agg: None }))
        .unwrap();
    assert_eq!(r.metrics().messages_rejected, 1);
}

#[test]
fn the_leaders_value_decided_after_the_first_step_is_not_a_fast_path_decision() {
    let (mut r, slot, own) = leading();
    let mut out = Vec::new();
    assert_eq!(own.priority, LEADER_PRIORITY);
    let wire = || Some(reply_proposal(&own, own.key));
    // Recorder 1 is already at step 6 with the leader's value first: the proposer adopts it.
    r.handle(
        r.now,
        from_peer(1, 0, Body::RecordReply { slot, req_step: 4, step: 6, first: wire(), prev_agg: None }),
    )
    .unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.proposers[&slot].prop.step(), 6);
    // Both remote recorders report it as the aggregate of step 5: decided at step 6.
    for from in [1, 2] {
        let reply = Body::RecordReply { slot, req_step: 6, step: 6, first: wire(), prev_agg: wire() };
        r.handle(r.now, from_peer(from, 0, reply)).unwrap();
        r.poll_actions(&mut out);
    }
    assert!(r.log.get(&slot).is_some_and(|d| d.proposal.key == own.key), "decided at step 6");
    assert_eq!(r.metrics().fast_path_decisions, 0);
}

#[test]
fn the_leader_takes_forwards_up_to_its_cap_and_no_more() {
    // cap = max_outstanding_proposals · members = 2 · 3.
    let accepted = |n: u64| {
        let mut r = replica_as(0);
        let mut out = Vec::new();
        r.cfg.max_outstanding_proposals = 2;
        let entries: Vec<Item> = (1..=n).map(|id| item(1, id, Bytes::from_static(b"x"))).collect();
        r.handle(Timestamp(1), from_peer(1, 0, Body::Forward { batch: batch::encode(&entries) })).unwrap();
        r.poll_actions(&mut out);
        r.pending.len() + r.batches.values().map(|b| b.entries.len()).sum::<usize>()
    };
    assert_eq!(accepted(6), 6, "a forward that fills the cap exactly is taken");
    assert_eq!(accepted(7), 0, "one past the cap is refused whole");
}

#[test]
fn a_forward_split_across_batches_counts_each_one() {
    let mut r = replica_as(1);
    let mut out = Vec::new();
    r.cfg.max_batch_bytes = 128;
    for id in 1..=2 {
        r.pending.push_back(item(1, id, Bytes::from(alloc::vec![0; 80])));
    }
    r.drive();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 2);
    assert_eq!(r.metrics().forwarded, 2);
}

#[test]
fn a_leader_decision_at_the_instant_a_batch_starts_is_not_progress_since() {
    // As `entries_a_leader_handed_over_go_back_to_it_once_it_decides_again`, but the leader's
    // own decision arrives at the instant the batch was proposed: not after it, so the entry
    // is proposed here again rather than handed back.
    let mut r = replica_as(1);
    let mut out = Vec::new();
    let entries = [item(2, 7, Bytes::from_static(b"x"))];
    r.handle(Timestamp(1), from_peer(0, 1, Body::Forward { batch: batch::encode(&entries) })).unwrap();
    r.poll_actions(&mut out);
    let slot = r.batches.values().find_map(|b| b.slot).expect("the entry is proposed here");
    let own = Body::Decided { slot: Slot(slot.0 + 1), proposal: proposal(1, 0, 1) };
    r.handle(Timestamp(1), from_peer(0, 1, own)).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.watch.progressed, Timestamp(1));
    out.clear();
    r.handle(Timestamp(3), from_peer(0, 1, Body::Decided { slot, proposal: proposal(1, 0, 0) })).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 0, "not handed back to the leader");
    assert!(
        r.batches.values().any(|b| b.slot.is_some() && b.entries.iter().any(|e| e.2 == ProposalId(7))),
        "proposed again in a slot of this replica's own"
    );
}

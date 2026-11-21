//! Proposer side: reply counters, the leader's forward cap, split forwards, and a lost
//! slot's entries.

use super::tests::{announce, from_peer, proposal, replica_as};
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
        r.entries.pending().len() + r.entries.batches().values().map(|b| b.entries.len()).sum::<usize>()
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
        r.entries.queue_pending(item(1, id, Bytes::from(alloc::vec![0; 80])));
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
    let slot = r.entries.batches().values().find_map(|b| b.slot).expect("the entry is proposed here");
    let own = Body::decided(Slot(slot.0 + 1), proposal(1, 0, 1));
    r.handle(Timestamp(1), from_peer(0, 1, own)).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.watch.progressed, Timestamp(1));
    out.clear();
    r.handle(Timestamp(3), from_peer(0, 1, Body::decided(slot, proposal(1, 0, 0)))).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 0, "not handed back to the leader");
    assert!(
        r.entries
            .batches()
            .values()
            .any(|b| b.slot.is_some() && b.entries.iter().any(|e| e.2 == ProposalId(7))),
        "proposed again in a slot of this replica's own"
    );
}

const MS: u64 = 1_000_000;

/// Leader 0 proposing replica 2's entry in slot 0 and its own in slot 1, steps aside once a
/// write has waited 4δ on its stuck disk while the others decide: both batches keep their
/// slots, and their entries go to replica 1 too. Returns the replica, its own entry, and
/// when it stepped aside. `check_entries` holds after every call.
fn aside_with_slots_in_flight() -> (Replica, ProposalId, Timestamp) {
    let mut r = replica_as(0);
    let mut out = Vec::new();
    r.commit_latency = Some(5 * MS);
    r.watch.disk.assume_usual(MS);
    let theirs = [item(2, 7, Bytes::from_static(b"theirs"))];
    r.handle(Timestamp(MS), from_peer(2, 0, Body::Forward { batch: batch::encode(&theirs) })).unwrap();
    r.poll_actions(&mut out);
    r.check_entries().unwrap();
    let own = r.propose(Timestamp(MS), Bytes::from_static(b"own")).unwrap();
    r.poll_actions(&mut out);
    r.check_entries().unwrap();
    let slots: Vec<_> = r.entries.batches().values().filter_map(|b| b.slot).collect();
    assert_eq!(slots, [Slot(0), Slot(1)], "each in a slot of its own");
    let elsewhere = Body::decided(Slot(9), proposal(1, 1, 9));
    r.handle(Timestamp(10 * MS), from_peer(1, 0, elsewhere)).unwrap();
    r.poll_actions(&mut out);
    let aside = Timestamp(MS + 80 * MS);
    r.handle_timeout(aside).unwrap();
    r.poll_actions(&mut out);
    r.check_entries().unwrap();
    assert_eq!(r.target(), NodeId(1), "stepped aside");
    assert_eq!(r.entries.forwarded().get(&own).map(|(_, at)| *at), Some(aside), "its own entry handed on");
    assert_eq!(r.entries.batches().values().filter(|b| b.slot.is_some()).count(), 2, "both slots kept");
    (r, own, aside)
}

/// Replica 1 decides `slot` with a value of its own.
fn lose(r: &mut Replica, slot: u64, at: Timestamp) {
    announce(r, at, 1, Slot(slot), proposal(1, 1, slot));
    r.check_entries().unwrap();
}

#[test]
fn entries_stay_tracked_when_a_stepped_aside_leader_loses_its_slots() {
    let (mut r, own, aside) = aside_with_slots_in_flight();
    lose(&mut r, 0, aside.after(5 * MS));
    lose(&mut r, 1, aside.after(5 * MS));
    assert!(r.entries.own().contains_key(&own));
}

/// #187 D2. Stepped aside, the leader already handed its slotted batches' entries on. When
/// it then loses those slots, replica 2's entry is not proposed here, on the stuck disk with
/// the whole window open: it went to replica 1 at stepping aside. Its own entry is handed on
/// once more, since replica 1 decided the slot without it, and then waits in that one place.
#[test]
fn a_stepped_aside_leader_losing_its_slots_proposes_nothing_and_hands_its_own_on_once() {
    let (mut r, own, aside) = aside_with_slots_in_flight();
    lose(&mut r, 0, aside.after(5 * MS));
    assert!(r.proposers.keys().all(|s| *s == Slot(1)), "nothing proposed here while stepped aside");
    let mut out = Vec::new();
    let at = aside.after(5 * MS);
    out.extend(announce(&mut r, at, 1, Slot(1), proposal(1, 1, 1)));
    r.check_entries().unwrap();
    assert!(r.proposers.is_empty(), "nothing proposed here while stepped aside");
    assert_eq!(forwards_to(&out, 1), 1, "its own entry handed on once more");
    assert_eq!(
        r.entries.forwarded().get(&own).map(|(_, t)| *t),
        Some(at),
        "handed on when the slot was lost"
    );
    assert!(!r.entries.pending().iter().any(|i| i.entry.id == own), "not also pending");
    assert!(!r.entries.relayed().iter().any(|i| i.entry.id == own), "not also relayed");
    assert!(r.entries.batches().values().all(|b| !b.entries.iter().any(|e| e.2 == own)), "in no batch");
}

/// Recorders 1 and 2 answer `slot`'s first step at `at` with `own` first: decided there.
fn decide_at_the_first_step(r: &mut Replica, at: Timestamp, slot: Slot, own: &Proposal) {
    let mut out = Vec::new();
    for from in [1, 2] {
        let first = Some(reply_proposal(own, own.key));
        let reply = Body::RecordReply { slot, req_step: 4, step: 4, first, prev_agg: None };
        r.handle(at, from_peer(from, 0, reply)).unwrap();
        r.poll_actions(&mut out);
    }
    assert!(r.log.contains_key(&slot), "decided at the first step");
}

/// Waits from each retransmission of `slot`'s current step to the next, in ms.
fn resend_gaps(r: &mut Replica, slot: Slot, n: usize) -> Vec<u64> {
    let mut out = Vec::new();
    let mut at = r.proposers[&slot].resend_at;
    (0..n)
        .map(|_| {
            r.handle_timeout(at).unwrap();
            r.poll_actions(&mut out);
            let next = r.proposers[&slot].resend_at;
            let gap = (next.0 - at.0) / MS;
            at = next;
            gap
        })
        .collect()
}

#[test]
fn resends_of_a_step_back_off_up_to_hedge_max() {
    let (mut r, slot, own) = leading();
    assert_eq!(r.proposers[&slot].resend_at, Timestamp(1).after(50 * MS), "retransmit_interval");
    // A request queued behind a saturated link is not lost: each resend waits twice as long.
    assert_eq!(resend_gaps(&mut r, slot, 6), [100, 200, 400, 800, 1000, 1000], "doubling to hedge_max");
    assert_eq!(r.metrics().retransmits, 6);
    // Recorder 1 is at step 6 already: the new step starts from the floor again.
    let first = Some(reply_proposal(&own, own.key));
    let now = r.now;
    let reply = Body::RecordReply { slot, req_step: 4, step: 6, first, prev_agg: None };
    r.handle(now, from_peer(1, 0, reply)).unwrap();
    r.poll_actions(&mut Vec::new());
    assert_eq!(r.proposers[&slot].prop.step(), 6);
    assert_eq!(r.proposers[&slot].resend_at, now.after(50 * MS), "a new step is not backed off");
    assert_eq!(resend_gaps(&mut r, slot, 2), [100, 200]);
}

#[test]
fn resends_wait_at_least_twice_the_first_step_round_trip() {
    let (mut r, slot, own) = leading();
    // Decided at the first step 40 ms after it started, never resent.
    let at = Timestamp(1 + 40 * MS);
    decide_at_the_first_step(&mut r, at, slot, &own);
    assert_eq!(r.request_rtt, Some(40 * MS));
    r.propose(at, Bytes::from_static(b"y")).unwrap();
    r.poll_actions(&mut Vec::new());
    let (&next, a) = r.proposers.iter().next().expect("the next slot is driven");
    assert_eq!(a.resend_at, at.after(80 * MS), "2 × 40 ms, above retransmit_interval");
    assert_eq!(resend_gaps(&mut r, next, 2), [160, 320]);
    // A faster slot pulls the round trip down slowly; a slower one raises it at once.
    let (mut r, slot, own) = leading();
    r.request_rtt = Some(80 * MS);
    decide_at_the_first_step(&mut r, Timestamp(1 + 40 * MS), slot, &own);
    assert_eq!(r.request_rtt, Some(80 * MS - 10 * MS + 5 * MS));
    let (mut r, slot, own) = leading();
    r.request_rtt = Some(10 * MS);
    decide_at_the_first_step(&mut r, Timestamp(1 + 40 * MS), slot, &own);
    assert_eq!(r.request_rtt, Some(40 * MS));
}

#[test]
fn a_slot_resent_before_its_decision_teaches_no_round_trip() {
    let (mut r, slot, own) = leading();
    assert_eq!(resend_gaps(&mut r, slot, 1), [100]);
    let at = r.now.after(MS);
    decide_at_the_first_step(&mut r, at, slot, &own);
    assert_eq!(r.request_rtt, None, "the answer may be to either copy of the request");
}

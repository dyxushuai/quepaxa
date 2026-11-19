//! Suspecting a silent leader, taking over its slots, and handing entries to a deputy.

use super::*;

#[test]
fn after_a_hedge_new_entries_are_proposed_here_not_forwarded() {
    let (mut r, _) = suspecting();
    let mut out = Vec::new();
    let next = r.propose(r.now, Bytes::from_static(b"y")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 0, "nothing more goes to the silent leader");
    assert!(!r.entries.forwarded().contains_key(&next));
    assert!(
        r.entries.batches().values().any(|b| b.slot.is_some() && b.entries.iter().any(|e| e.2 == next)),
        "the new entry is proposed in a slot of this replica's own"
    );
}

#[test]
fn suspecting_hedges_every_entry_already_forwarded() {
    let mut r = replica_as(1);
    let mut out = Vec::new();
    r.commit_latency = Some(20 * MS);
    r.watch.reply.insert(NodeId(0), 5 * MS);
    r.cfg.hedge_max = 80 * MS;
    let first = r.propose(Timestamp(1), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    // A second entry forwarded later: its own hedge deadline is further away.
    let second = r.propose(Timestamp(1 + 30 * MS), Bytes::from_static(b"y")).unwrap();
    r.poll_actions(&mut out);
    while r.entries.forwarded().contains_key(&first) {
        let at = r.next_deadline().expect("the hedge is scheduled");
        r.handle_timeout(at).unwrap();
        r.poll_actions(&mut out);
    }
    assert!(!r.entries.forwarded().contains_key(&second), "suspicion covers what was handed to the leader");
    assert!(r.entries.own().get(&second).is_some_and(|o| o.hedged), "and counts it as hedged");
    assert_eq!(r.metrics().hedged, 2, "both entries count in the metric");
}

#[test]
fn a_suspecting_follower_proposes_a_lost_batch_again_itself() {
    let (mut r, id) = suspecting();
    let mut out = Vec::new();
    let slot = r.entries.batches().values().find_map(|b| b.slot).expect("the hedged batch has a slot");
    // Another follower's value takes the slot (the leader is still silent).
    r.handle(r.now, from_peer(2, 1, Body::Decided { slot, proposal: proposal(1, 2, 0) })).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 0, "not handed back to the silent leader");
    assert!(
        r.entries.batches().values().any(|b| b.entries.iter().any(|e| e.2 == id)),
        "the entry stays in a batch of this replica's own"
    );
}

#[test]
fn the_dead_leaders_own_value_deciding_does_not_end_the_suspicion() {
    let (mut r, _) = suspecting();
    let mut out = Vec::new();
    // A slot the leader started, taken over by another follower: it decides with the
    // leader's value, but the leader itself said nothing.
    r.handle(r.now, from_peer(2, 1, Body::Decided { slot: Slot(0), proposal: proposal(1, 0, 7) })).unwrap();
    r.poll_actions(&mut out);
    let next = r.propose(r.now, Bytes::from_static(b"y")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 0);
    assert!(!r.entries.forwarded().contains_key(&next));
}

#[test]
fn hearing_from_the_leader_again_ends_the_suspicion() {
    let (mut r, _) = suspecting();
    let mut out = Vec::new();
    r.handle(
        r.now,
        from_peer(0, 1, Body::RecordRequest { slot: Slot(5), step: 4, proposal: proposal(1, 0, 9) }),
    )
    .unwrap();
    r.poll_actions(&mut out);
    out.clear();
    let next = r.propose(r.now, Bytes::from_static(b"y")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 1, "the leader is alive: forward to it again");
    assert!(r.entries.forwarded().contains_key(&next));
}

#[test]
fn a_silent_leaders_slot_is_taken_over_after_sigma() {
    let mut r = with_latency(Some(5));
    let mut out = Vec::new();
    // The leader starts slot 0, then dies: its request is all this replica hears.
    let request = Body::RecordRequest { slot: Slot(0), step: 4, proposal: proposal(1, 0, 9) };
    r.handle(Timestamp(MS), from_peer(0, 1, request)).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.next_deadline(), Some(Timestamp(MS + 20 * MS)), "position 1 × σ, not 2δ");
}

#[test]
fn a_follower_behind_the_deputy_hands_its_entries_to_the_deputy() {
    // Replica 2 of three: once it suspects leader 0, replica 1 is next in the order.
    let mut r = replica_as(2);
    let mut out = Vec::new();
    r.commit_latency = Some(20 * MS);
    r.watch.reply.insert(NodeId(0), 5 * MS);
    r.cfg.hedge_max = 80 * MS;
    let id = r.propose(Timestamp(1), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 1);
    out.clear();
    let at = r.next_deadline().expect("the hedge is scheduled");
    r.handle_timeout(at).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 1), 1, "everything waiting goes to the deputy");
    assert!(
        r.entries.batches().values().all(|b| b.slot.is_none()),
        "not proposed here: one proposer at a time"
    );
    assert!(r.entries.forwarded().contains_key(&id));
}

#[test]
fn a_deputy_not_yet_heard_from_is_waited_for_the_hedge_delay() {
    // R was measured against the leader; the deputy may be farther away.
    let mut r = replica_as(2);
    let mut out = Vec::new();
    r.commit_latency = Some(20 * MS);
    r.watch.reply.insert(NodeId(0), 5 * MS);
    r.cfg.hedge_max = 80 * MS;
    r.propose(Timestamp(1), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    let at = r.next_deadline().expect("the hedge is scheduled");
    r.handle_timeout(at).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.watch.asked.map(|(to, _)| to), Some(NodeId(1)), "silence now counts for the deputy");
    assert_eq!(r.next_deadline(), Some(at.after(80 * MS)), "δ, not the leader's σ");
}

#[test]
fn a_deputy_proposes_what_it_is_handed() {
    let mut r = replica_as(1);
    let mut out = Vec::new();
    let entries = [batch::Item {
        entry: crate::batch::Entry {
            origin: NodeId(2),
            incarnation: 1,
            id: ProposalId(7),
            payload: Bytes::from_static(b"x"),
        },
        kind: batch::Kind::App,
    }];
    let forward = Body::Forward { batch: batch::encode(&entries) };
    r.handle(Timestamp(1), from_peer(2, 1, forward)).unwrap();
    r.poll_actions(&mut out);
    assert!(
        r.entries
            .batches()
            .values()
            .any(|b| b.slot.is_some() && b.entries.iter().any(|e| e.2 == ProposalId(7))),
        "replica 2 suspects the leader; its deputy proposes the entry"
    );
}

#[test]
fn a_deputy_proposes_a_handed_entry_again_when_its_batch_loses_the_slot() {
    let mut r = replica_as(1);
    let mut out = Vec::new();
    let entries = [batch::Item {
        entry: crate::batch::Entry {
            origin: NodeId(2),
            incarnation: 1,
            id: ProposalId(7),
            payload: Bytes::from_static(b"x"),
        },
        kind: batch::Kind::App,
    }];
    r.handle(Timestamp(1), from_peer(2, 1, Body::Forward { batch: batch::encode(&entries) })).unwrap();
    r.poll_actions(&mut out);
    let slot = r.entries.batches().values().find_map(|b| b.slot).expect("the deputy proposes the entry");
    out.clear();
    // Another value takes the slot; this replica itself never suspected leader 0.
    r.handle(r.now, from_peer(0, 1, Body::Decided { slot, proposal: proposal(1, 0, 0) })).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 0, "not handed to the leader replica 2 gave up on");
    assert!(
        r.entries
            .batches()
            .values()
            .any(|b| b.slot.is_some() && b.entries.iter().any(|e| e.2 == ProposalId(7))),
        "proposed again in a slot of the deputy's own"
    );
}

#[test]
fn entries_a_leader_handed_over_go_back_to_it_once_it_decides_again() {
    // Leader 0 stepped aside for a moment and handed replica 2's entry here; it is back,
    // deciding slots itself, and takes the slot this replica proposed the entry in.
    let mut r = replica_as(1);
    let mut out = Vec::new();
    let entries = [batch::Item {
        entry: crate::batch::Entry {
            origin: NodeId(2),
            incarnation: 1,
            id: ProposalId(7),
            payload: Bytes::from_static(b"x"),
        },
        kind: batch::Kind::App,
    }];
    r.handle(Timestamp(1), from_peer(0, 1, Body::Forward { batch: batch::encode(&entries) })).unwrap();
    r.poll_actions(&mut out);
    let slot = r.entries.batches().values().find_map(|b| b.slot).expect("the entry is proposed here");
    let own = Body::Decided { slot: Slot(slot.0 + 1), proposal: proposal(1, 0, 1) };
    r.handle(Timestamp(2), from_peer(0, 1, own)).unwrap();
    r.poll_actions(&mut out);
    out.clear();
    r.handle(Timestamp(3), from_peer(0, 1, Body::Decided { slot, proposal: proposal(1, 0, 0) })).unwrap();
    r.poll_actions(&mut out);
    assert!(r.entries.batches().is_empty(), "not proposed again, to lose to the leader slot after slot");
    assert_eq!(forwards_to(&out, 0), 1, "handed back to the leader");
}

#[test]
fn entries_handed_to_a_leader_this_replica_now_is_are_not_left_waiting() {
    // Replica 1 forwarded to leader 0, then came to lead itself (here: 0 suspected and
    // 1 first in line): the late entries must leave `forwarded`, or their timer fires
    // forever.
    let mut r = replica_as(1);
    let mut out = Vec::new();
    r.commit_latency = Some(20 * MS);
    let id = r.propose(Timestamp(1), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    r.watch.suspects.insert(NodeId(0), Timestamp(1));
    let at = r.next_deadline().expect("the hedge is scheduled");
    r.handle_timeout(at).unwrap();
    r.poll_actions(&mut out);
    assert!(!r.entries.forwarded().contains_key(&id));
    assert!(r.next_deadline().is_none_or(|t| t > at), "no timer left due");
}

#[test]
fn changing_target_does_not_reuse_an_old_forwards_silence_deadline() {
    let mut r = replica_as(2);
    let mut out = Vec::new();
    r.commit_latency = Some(20 * MS);
    r.cfg.hedge_max = 80 * MS;
    r.watch.reply.insert(NodeId(0), 5 * MS);
    r.watch.reply.insert(NodeId(1), 5 * MS);
    let old_at = Timestamp(MS);
    r.propose(old_at, Bytes::from_static(b"old target")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 1);
    r.schedule = crate::schedule::Schedule::with_order(alloc::vec![NodeId(1), NodeId(0), NodeId(2)]);
    assert_eq!(r.silent_at(old_at), None, "nothing has been forwarded to the new target");
    let new_at = Timestamp(10 * MS);
    r.propose(new_at, Bytes::from_static(b"new target")).unwrap();
    out.clear();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 1), 1);
    assert_eq!(r.silent_at(old_at), Some(new_at.after(20 * MS)));
}

#[test]
fn a_leader_still_talking_keeps_its_slow_slot() {
    let mut r = with_latency(Some(5));
    let mut out = Vec::new();
    let request = Body::RecordRequest { slot: Slot(0), step: 4, proposal: proposal(1, 0, 9) };
    r.handle(Timestamp(MS), from_peer(0, 1, request)).unwrap();
    r.poll_actions(&mut out);
    // The slot is slow to decide, but the leader keeps sending (its other slots).
    r.handle(Timestamp(15 * MS), from_peer(0, 1, Body::ReadQuery { incarnation: 1, round: 0 })).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(
        r.next_deadline(),
        Some(Timestamp(35 * MS)),
        "σ after the leader last spoke, not after the slot"
    );
}

#[test]
fn each_suspicion_is_counted_once() {
    let (r, _) = suspecting();
    assert_eq!(r.metrics().suspicions, 1);
}

//! A follower's watch on its leader: suspecting a silent one and proposing directly.

use super::disk::Health;
use super::tests::{from_peer, proposal, r_wire, replica_as};
use super::*;

const MS: u64 = 1_000_000;

/// Entries `r` handed to `leader` among `out`.
pub(super) fn forwards_to(out: &[Action], leader: u32) -> usize {
    out.iter()
        .filter(|a| match a {
            Action::Send(o) => {
                o.to == NodeId(leader)
                    && matches!(
                        Message::decode(&o.bytes, &r_wire()),
                        Ok(Message { body: Body::Forward { .. }, .. })
                    )
            }
            _ => false,
        })
        .count()
}

/// Replica 1 of three with a forwarded entry the silent leader (0) never commits, run
/// until the follower hedges it.
fn suspecting() -> (Replica, ProposalId) {
    let mut r = replica_as(1);
    let mut out = Vec::new();
    r.commit_latency = Some(20 * MS);
    r.watch.reply.insert(NodeId(0), 5 * MS);
    r.cfg.hedge_max = 80 * MS;
    let id = r.propose(Timestamp(1), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 1);
    while r.forwarded.contains_key(&id) {
        let at = r.next_deadline().expect("the hedge is scheduled");
        r.handle_timeout(at).unwrap();
        r.poll_actions(&mut out);
    }
    (r, id)
}

#[test]
fn after_a_hedge_new_entries_are_proposed_here_not_forwarded() {
    let (mut r, _) = suspecting();
    let mut out = Vec::new();
    let next = r.propose(r.now, Bytes::from_static(b"y")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 0, "nothing more goes to the silent leader");
    assert!(!r.forwarded.contains_key(&next));
    assert!(
        r.batches.values().any(|b| b.slot.is_some() && b.entries.iter().any(|e| e.2 == next)),
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
    while r.forwarded.contains_key(&first) {
        let at = r.next_deadline().expect("the hedge is scheduled");
        r.handle_timeout(at).unwrap();
        r.poll_actions(&mut out);
    }
    assert!(!r.forwarded.contains_key(&second), "suspicion covers what was handed to the leader");
    assert!(r.outstanding.get(&second).is_some_and(|o| o.2), "and counts it as hedged");
}

#[test]
fn a_suspecting_follower_proposes_a_lost_batch_again_itself() {
    let (mut r, id) = suspecting();
    let mut out = Vec::new();
    let slot = r.batches.values().find_map(|b| b.slot).expect("the hedged batch has a slot");
    // Another follower's value takes the slot (the leader is still silent).
    r.handle(r.now, from_peer(2, 1, Body::Decided { slot, proposal: proposal(1, 2, 0) })).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 0, "not handed back to the silent leader");
    assert!(
        r.batches.values().any(|b| b.entries.iter().any(|e| e.2 == id)),
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
    assert!(!r.forwarded.contains_key(&next));
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
    assert!(r.forwarded.contains_key(&next));
}

/// Replica 1 with a 20 ms commit latency (δ = 80 ms) and, when given, an observed delay R
/// from a forward to the leader's next message.
fn with_latency(reply_ms: Option<u64>) -> Replica {
    let mut r = replica_as(1);
    r.cfg.retransmit_interval = 10 * MS;
    r.cfg.hedge_max = 80 * MS;
    r.commit_latency = Some(20 * MS);
    r.watch.reply.extend(reply_ms.map(|ms| (NodeId(0), ms * MS)));
    r
}

#[test]
fn an_entry_is_hedged_after_sigma_when_the_leader_says_nothing() {
    let mut r = with_latency(Some(5));
    let mut out = Vec::new();
    r.propose(Timestamp(MS), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.next_deadline(), Some(Timestamp(MS + 20 * MS)), "σ = max(20 ms, 2 × 5 ms)");
    // Without R, nothing is early.
    let mut plain = with_latency(None);
    plain.propose(Timestamp(MS), Bytes::from_static(b"x")).unwrap();
    plain.poll_actions(&mut out);
    assert_eq!(plain.next_deadline(), Some(Timestamp(MS + 80 * MS)), "δ");
}

#[test]
fn a_leader_that_speaks_after_the_forward_is_waited_for_the_hedge_delay() {
    let mut r = with_latency(Some(5));
    let mut out = Vec::new();
    r.propose(Timestamp(MS), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    // Any message will do; a read query starts no slot of its own.
    r.handle(Timestamp(4 * MS), from_peer(0, 1, Body::ReadQuery { incarnation: 1, round: 0 })).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.next_deadline(), Some(Timestamp(MS + 80 * MS)));
    // The 3 ms from the forward to the leader's message is learned as R.
    assert_eq!(r.watch.reply.get(&NodeId(0)), Some(&(5 * MS - 5 * MS / 8 + 3 * MS / 8)));
}

#[test]
fn an_idle_spell_before_the_forward_is_not_silence() {
    let mut r = with_latency(Some(5));
    let mut out = Vec::new();
    // The leader spoke long ago, then the cluster idled.
    r.handle(Timestamp(MS), from_peer(0, 1, Body::ReadQuery { incarnation: 1, round: 0 })).unwrap();
    r.poll_actions(&mut out);
    let at = Timestamp(10_000 * MS);
    r.propose(at, Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.next_deadline(), Some(at.after(20 * MS)), "silence counts from the forward");
    r.handle(at.after(3 * MS), from_peer(0, 1, Body::ReadQuery { incarnation: 1, round: 1 })).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.next_deadline(), Some(at.after(80 * MS)), "the leader answered in time");
}

#[test]
fn sigma_counts_only_below_the_hedge_delay_and_covers_twice_r() {
    let mut r = with_latency(Some(60));
    assert_eq!(r.silence_wait(NodeId(0)), None, "2R past δ: silence says nothing δ does not");
    r.cfg.hedge_max = 400 * MS;
    r.commit_latency = Some(40 * MS);
    assert_eq!(r.silence_wait(NodeId(0)), Some(120 * MS), "a leader slow to answer (R = 60 ms) is given 2R");
    r.watch.reply.insert(NodeId(0), MS);
    r.cfg.retransmit_interval = 400 * MS;
    assert_eq!(
        r.silence_wait(NodeId(0)),
        Some(MS + 50 * MS),
        "R and the wait for a rate-limited ack (retransmit_interval / 8)"
    );
    r.cfg.retransmit_interval = 10 * MS;
    r.commit_latency = Some(5 * MS);
    r.watch.reply.insert(NodeId(0), MS / 100);
    assert_eq!(r.silence_wait(NodeId(0)), Some(5 * MS), "a quick reply leaves the commit latency");
    r.cfg.hedge_max = 2 * MS;
    r.cfg.retransmit_interval = MS / 100;
    r.commit_latency = Some(MS);
    assert_eq!(r.silence_wait(NodeId(0)), Some(r.cfg.hedge_min), "nor below hedge_min");
    r.watch.reply.clear();
    assert_eq!(r.silence_wait(NodeId(0)), None);
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
    assert!(r.batches.values().all(|b| b.slot.is_none()), "not proposed here: one proposer at a time");
    assert!(r.forwarded.contains_key(&id));
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
        r.batches.values().any(|b| b.slot.is_some() && b.entries.iter().any(|e| e.2 == ProposalId(7))),
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
    let slot = r.batches.values().find_map(|b| b.slot).expect("the deputy proposes the entry");
    out.clear();
    // Another value takes the slot; this replica itself never suspected leader 0.
    r.handle(r.now, from_peer(0, 1, Body::Decided { slot, proposal: proposal(1, 0, 0) })).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 0, "not handed to the leader replica 2 gave up on");
    assert!(
        r.batches.values().any(|b| b.slot.is_some() && b.entries.iter().any(|e| e.2 == ProposalId(7))),
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
    let slot = r.batches.values().find_map(|b| b.slot).expect("the entry is proposed here");
    let own = Body::Decided { slot: Slot(slot.0 + 1), proposal: proposal(1, 0, 1) };
    r.handle(Timestamp(2), from_peer(0, 1, own)).unwrap();
    r.poll_actions(&mut out);
    out.clear();
    r.handle(Timestamp(3), from_peer(0, 1, Body::Decided { slot, proposal: proposal(1, 0, 0) })).unwrap();
    r.poll_actions(&mut out);
    assert!(r.batches.is_empty(), "not proposed again, to lose to the leader slot after slot");
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
    assert!(!r.forwarded.contains_key(&id));
    assert!(r.next_deadline().is_none_or(|t| t > at), "no timer left due");
}

#[test]
fn a_slow_but_talking_leader_is_hedged_around_entry_by_entry_and_not_suspected() {
    let mut r = with_latency(Some(5));
    let mut out = Vec::new();
    let id = r.propose(Timestamp(MS), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    r.handle(Timestamp(4 * MS), from_peer(0, 1, Body::ReadQuery { incarnation: 1, round: 0 })).unwrap();
    r.poll_actions(&mut out);
    r.handle_timeout(Timestamp(MS + 80 * MS)).unwrap();
    r.poll_actions(&mut out);
    assert!(
        r.batches.values().any(|b| b.slot.is_some() && b.entries.iter().any(|e| e.2 == id)),
        "after δ the entry is proposed here"
    );
    out.clear();
    let next = r.propose(r.now, Bytes::from_static(b"y")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 1, "the leader was talking: not suspected");
    assert!(r.forwarded.contains_key(&next));
}

#[test]
fn a_leader_working_through_a_backlog_is_hedged_around_only_after_4_delta() {
    let mut r = with_latency(Some(5));
    let mut out = Vec::new();
    // δ = 80 ms; 4δ stays under hedge_max.
    r.cfg.hedge_max = 400 * MS;
    let id = r.propose(Timestamp(MS), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    // A value of the leader's own is decided after the forward: it is busy, not stuck.
    r.handle(
        Timestamp(4 * MS),
        from_peer(0, 1, Body::Decided { slot: Slot(0), proposal: proposal(1, 0, 0) }),
    )
    .unwrap();
    r.poll_actions(&mut out);
    // Its disk works: the decision's record is written.
    let last = r.watch.disk.in_flight().next_back().expect("the decision is being written");
    r.handle(Timestamp(5 * MS), Input::Persisted(PersistId(last))).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.next_deadline(), Some(Timestamp(MS + 4 * 80 * MS)), "4δ, not δ");
    r.handle_timeout(Timestamp(MS + 80 * MS)).unwrap();
    r.poll_actions(&mut out);
    assert!(r.forwarded.contains_key(&id), "still waiting for the leader after δ");
    r.handle_timeout(Timestamp(MS + 4 * 80 * MS)).unwrap();
    r.poll_actions(&mut out);
    assert!(!r.forwarded.contains_key(&id), "hedged after 4δ, in case the forward was lost");
}

#[test]
fn only_a_new_decision_the_target_announces_counts_as_its_progress() {
    let mut r = with_latency(Some(5));
    let mut out = Vec::new();
    let decided = |slot| Body::Decided { slot: Slot(slot), proposal: proposal(1, 2, slot) };
    // A decision this replica already holds, repeated by the leader.
    r.handle(Timestamp(MS), from_peer(2, 1, decided(0))).unwrap();
    r.poll_actions(&mut out);
    r.handle(Timestamp(2 * MS), from_peer(0, 1, decided(0))).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.watch.progressed, Timestamp(0), "a repeat");
    // Another replica's decision: not the leader deciding.
    r.handle(Timestamp(3 * MS), from_peer(2, 1, decided(1))).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.watch.progressed, Timestamp(0));
    r.handle(Timestamp(4 * MS), from_peer(0, 1, decided(2))).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.watch.progressed, Timestamp(4 * MS), "a slot the leader decided");
}

#[test]
fn a_deputy_drives_the_whole_window_for_what_it_is_handed() {
    let mut r = with_latency(Some(5));
    let mut out = Vec::new();
    r.cfg.max_batch_bytes = 128;
    let entries: Vec<_> = (0..4u8)
        .map(|i| batch::Item {
            entry: crate::batch::Entry {
                origin: NodeId(2),
                incarnation: 1,
                id: ProposalId(u64::from(i) + 1),
                payload: Bytes::from(alloc::vec![i; 80]),
            },
            kind: batch::Kind::App,
        })
        .collect();
    for e in &entries {
        let forward = Body::Forward { batch: batch::encode(core::slice::from_ref(e)) };
        r.handle(Timestamp(MS), from_peer(2, 1, forward)).unwrap();
        r.poll_actions(&mut out);
    }
    assert!(r.proposers.len() > 1, "not one slot at a time: these are not hedges");
}

#[test]
fn hedging_around_a_talking_leader_drives_one_slot_at_a_time() {
    let mut r = with_latency(Some(5));
    let mut out = Vec::new();
    r.cfg.max_batch_bytes = 128;
    for i in 0..4u8 {
        r.propose(Timestamp(MS), Bytes::from(alloc::vec![i; 80])).unwrap();
        r.poll_actions(&mut out);
    }
    r.handle(Timestamp(4 * MS), from_peer(0, 1, Body::ReadQuery { incarnation: 1, round: 0 })).unwrap();
    r.poll_actions(&mut out);
    r.handle_timeout(Timestamp(MS + 80 * MS)).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.proposers.len(), 1, "one hedged batch is proposed");
    assert!(!r.relayed.is_empty(), "the rest wait for its slot, though they fill more batches");
}

#[test]
fn r_rises_at_once_to_a_slower_answer() {
    let mut r = with_latency(Some(5));
    let mut out = Vec::new();
    r.propose(Timestamp(MS), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    r.handle(Timestamp(MS + 30 * MS), from_peer(0, 1, Body::ReadQuery { incarnation: 1, round: 0 })).unwrap();
    assert_eq!(
        r.watch.reply.get(&NodeId(0)),
        Some(&(30 * MS)),
        "a leader whose disk slowed: σ covers it at once"
    );
}

#[test]
fn without_silent_leader_takeover_a_silent_leader_is_waited_for_the_hedge_delay() {
    let mut r = with_latency(Some(5));
    r.cfg.silent_leader_takeover = false;
    let mut out = Vec::new();
    r.propose(Timestamp(MS), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.next_deadline(), Some(Timestamp(MS + 80 * MS)), "δ, as before the option");
    r.handle_timeout(Timestamp(MS + 80 * MS)).unwrap();
    r.poll_actions(&mut out);
    out.clear();
    let next = r.propose(r.now, Bytes::from_static(b"y")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 1, "the leader is not suspected");
    assert!(r.forwarded.contains_key(&next));
}

#[test]
fn a_leader_acknowledges_a_forward_before_its_writes_are_durable() {
    let mut r = replica_as(0);
    let mut out = Vec::new();
    // A write of its own in flight: the leader's disk is busy.
    r.propose(Timestamp(1), Bytes::from_static(b"own")).unwrap();
    r.poll_actions(&mut out);
    assert!(out.iter().any(|a| matches!(a, Action::Persist { .. })), "a write is in flight");
    out.clear();
    let entries = [batch::Item {
        entry: crate::batch::Entry {
            origin: NodeId(1),
            incarnation: 1,
            id: ProposalId(3),
            payload: Bytes::from_static(b"x"),
        },
        kind: batch::Kind::App,
    }];
    r.handle(Timestamp(2), from_peer(1, 0, Body::Forward { batch: batch::encode(&entries) })).unwrap();
    r.poll_actions(&mut out);
    let acked = out.iter().any(|a| match a {
        Action::Send(o) => {
            o.to == NodeId(1)
                && matches!(Message::decode(&o.bytes, &r_wire()), Ok(Message { body: Body::ForwardAck, .. }))
        }
        _ => false,
    });
    assert!(acked, "the ack does not wait for the disk: {out:?}");
    // Another forward within an eighth of the retransmit interval goes unacknowledged.
    let acks = |r: &mut Replica, at: u64| {
        let mut out = Vec::new();
        r.handle(Timestamp(at), from_peer(1, 0, Body::Forward { batch: batch::encode(&entries) })).unwrap();
        r.poll_actions(&mut out);
        out.iter()
            .filter(|a| matches!(a, Action::Send(o) if matches!(Message::decode(&o.bytes, &r_wire()), Ok(Message { body: Body::ForwardAck, .. }))))
            .count()
    };
    let every = r.cfg.retransmit_interval / 8;
    assert_eq!(acks(&mut r, 2 + every - 1), 0);
    assert_eq!(acks(&mut r, 2 + every), 1);
    assert_eq!(acks(&mut r, 3 + every), 0);
    let due = Timestamp(2 + 2 * every);
    assert!(r.next_deadline().is_some_and(|at| at <= due), "the suppressed ack has a timer");
    out.clear();
    r.handle_timeout(due).unwrap();
    r.poll_actions(&mut out);
    assert!(
        out.iter().any(|a| matches!(a, Action::Send(o) if o.to == NodeId(1)
        && matches!(Message::decode(&o.bytes, &r_wire()), Ok(Message { body: Body::ForwardAck, .. })))),
        "a suppressed ack is eventually sent without another forward or durable write"
    );
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

/// Leader 0 with a write stuck on its disk since 1 ms (usual writes take 1 ms; δ = 20 ms),
/// and an entry of its own waiting in a batch.
pub(super) fn leader_with_a_stuck_disk() -> (Replica, ProposalId) {
    let mut r = replica_as(0);
    let mut out = Vec::new();
    r.commit_latency = Some(5 * MS);
    r.watch.disk.assume_usual(MS);
    let id = r.propose(Timestamp(MS), Bytes::from_static(b"own")).unwrap();
    r.poll_actions(&mut out);
    assert!(out.iter().any(|a| matches!(a, Action::Persist { .. })), "a write is in flight");
    (r, id)
}

#[test]
fn a_leader_whose_disk_is_stuck_hands_its_entries_on_while_others_decide() {
    let (mut r, id) = leader_with_a_stuck_disk();
    let mut out = Vec::new();
    // The two others decide a slot without it: their disks work.
    let decided = Body::Decided { slot: Slot(9), proposal: proposal(1, 1, 0) };
    r.handle(Timestamp(10 * MS), from_peer(1, 0, decided)).unwrap();
    r.poll_actions(&mut out);
    r.handle_timeout(Timestamp(MS + 79 * MS)).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.target(), NodeId(0), "not before the write has waited 4δ");
    assert!(r.next_deadline().is_some_and(|t| t <= Timestamp(MS + 80 * MS)), "a timer for it");
    out.clear();
    r.handle_timeout(Timestamp(MS + 80 * MS)).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.target(), NodeId(1), "stepped aside for the next in the leader order");
    assert_eq!(forwards_to(&out, 1), 1, "its waiting entry goes there");
    assert!(r.forwarded.contains_key(&id));
    assert!(r.outstanding.get(&id).is_some_and(|o| o.2), "its wait does not count as a commit latency");
    // A forward now gets no acknowledgement: the followers move on too.
    out.clear();
    let entries = [batch::Item {
        entry: crate::batch::Entry {
            origin: NodeId(2),
            incarnation: 1,
            id: ProposalId(3),
            payload: Bytes::from_static(b"x"),
        },
        kind: batch::Kind::App,
    }];
    r.handle(r.now, from_peer(2, 0, Body::Forward { batch: batch::encode(&entries) })).unwrap();
    r.poll_actions(&mut out);
    let acked = out.iter().any(|a| matches!(a, Action::Send(o) if matches!(Message::decode(&o.bytes, &r_wire()), Ok(Message { body: Body::ForwardAck, .. }))));
    assert!(!acked, "no acknowledgement while stepped aside");
    // The forward to the deputy was lost: still uncommitted after δ, it is handed over again
    // (not proposed here, where the slots are stuck).
    out.clear();
    r.handle_timeout(r.now.after(20 * MS)).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 1), 1, "forwarded to the deputy again");
    assert!(r.relayed.is_empty(), "not hedged here");
    // Its disk catches up: it leads again.
    let last = r.watch.disk.in_flight().next_back().expect("writes in flight");
    r.handle(r.now, Input::Persisted(PersistId(last))).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.target(), NodeId(0));
}

#[test]
fn a_leader_does_not_step_aside_when_nobody_else_decides_either() {
    let (mut r, _) = leader_with_a_stuck_disk();
    let mut out = Vec::new();
    // Every disk may be stuck (one shared disk): stepping aside would only add contention.
    r.handle_timeout(Timestamp(MS + 200 * MS)).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.target(), NodeId(0));
    assert_eq!(forwards_to(&out, 1), 0);
}

#[test]
fn a_follower_whose_disk_is_stuck_does_not_step_aside() {
    // Its slots hold nobody up, and stepping aside would stop it hedging around a slow leader.
    let mut r = with_latency(Some(5));
    let mut out = Vec::new();
    r.watch.disk.assume_usual(MS);
    r.propose(Timestamp(MS), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    let decided = Body::Decided { slot: Slot(9), proposal: proposal(1, 0, 0) };
    r.handle(Timestamp(10 * MS), from_peer(0, 1, decided)).unwrap();
    r.poll_actions(&mut out);
    assert!(r.watch.disk.in_flight().len() > 0, "a write is stuck");
    assert_eq!(r.aside_at(), None);
    r.handle_timeout(Timestamp(MS + 200 * MS)).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.metrics.stepped_aside, 0);
}

/// Follower 1 (δ = 80 ms) recording the leader's request at 1 ms, a write its disk does not
/// finish, and an entry of its own forwarded at 2 ms.
fn follower_with_a_stuck_disk() -> (Replica, ProposalId) {
    let mut r = with_latency(None);
    let mut out = Vec::new();
    r.watch.disk.assume_usual(MS);
    let request = Body::RecordRequest { slot: Slot(0), step: 4, proposal: proposal(1, 0, 0) };
    r.handle(Timestamp(MS), from_peer(0, 1, request)).unwrap();
    r.poll_actions(&mut out);
    assert!(r.watch.disk.in_flight().len() > 0, "a write is in flight");
    let id = r.propose(Timestamp(2 * MS), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 1);
    (r, id)
}

#[test]
fn a_follower_does_not_hedge_while_every_disk_seems_stuck() {
    // One shared disk: a hedge cannot be decided before the leader's slots, and once the
    // disks resume it would contend with them.
    let (mut r, id) = follower_with_a_stuck_disk();
    let mut out = Vec::new();
    // A decision made durable before the disks stopped, arriving before the write was due
    // (four usual write times), says nothing about them.
    let decided = Body::Decided { slot: Slot(9), proposal: proposal(1, 2, 0) };
    r.handle(Timestamp(3 * MS), from_peer(2, 1, decided)).unwrap();
    r.poll_actions(&mut out);
    r.handle_timeout(Timestamp(150 * MS)).unwrap();
    r.poll_actions(&mut out);
    assert!(r.forwarded.contains_key(&id), "not hedged past δ");
    assert_eq!(r.metrics.hedged, 0);
    // The disk catches up: δ counts from then.
    let last = r.watch.disk.in_flight().next_back().expect("writes in flight");
    r.handle(Timestamp(150 * MS), Input::Persisted(PersistId(last))).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.late_at(Timestamp(2 * MS)), Some(Timestamp(230 * MS)));
    r.handle_timeout(Timestamp(230 * MS)).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.metrics.hedged, 1);
}

#[test]
fn a_disk_that_keeps_completing_writes_is_slow_not_stuck() {
    // One shared disk, slow: a write waits for the flush in progress and then its own, so
    // the oldest one in flight waits δ although the disk keeps completing writes. Holding
    // hedges there only fired them later, still around the slow leader.
    let mut r = with_latency(None);
    let mut out = Vec::new();
    r.watch.disk.assume_usual(MS);
    for (slot, at) in [(0, 2), (1, 3)] {
        let request = Body::RecordRequest { slot: Slot(slot), step: 4, proposal: proposal(1, 0, slot) };
        r.handle(Timestamp(at * MS / 2), from_peer(0, 1, request)).unwrap();
        r.poll_actions(&mut out);
    }
    assert_eq!(r.watch.disk.in_flight().len(), 2, "two writes in flight");
    let id = r.propose(Timestamp(2 * MS), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 1);
    // The first write completes, late; the second is still in flight past δ.
    let first = r.watch.disk.in_flight().next().expect("writes in flight");
    r.handle(Timestamp(40 * MS), Input::Persisted(PersistId(first))).unwrap();
    r.poll_actions(&mut out);
    assert!(r.watch.disk.in_flight().len() > 0, "the second write is still in flight");
    assert_eq!(r.late_at(Timestamp(2 * MS)), Some(Timestamp(82 * MS)), "not held, δ not restarted");
    r.handle_timeout(Timestamp(82 * MS)).unwrap();
    r.poll_actions(&mut out);
    assert!(!r.forwarded.contains_key(&id), "hedged after δ");
    assert_eq!(r.metrics.hedged, 1);
}

#[test]
fn a_hedge_stays_scheduled_until_the_disk_is_actually_stuck() {
    // A write in flight, nobody deciding: the disk would seem stuck at 81 ms, before the
    // hedge at 82 ms, if no write completed meanwhile. That is a forecast a slow disk keeps
    // pushing back; holding on it left the entry with no timer at all.
    let (mut r, id) = follower_with_a_stuck_disk();
    let mut out = Vec::new();
    let d = r.hedge_delay();
    assert_eq!(r.watch.disk.health(Timestamp(81 * MS - 1), d), Health::Writing);
    assert_eq!(r.watch.disk.health(Timestamp(81 * MS), d), Health::SharedStall { since: Timestamp(81 * MS) });
    assert_eq!(r.late_at(Timestamp(2 * MS)), Some(Timestamp(82 * MS)), "the hedge is scheduled");
    assert!(r.next_deadline().is_some_and(|t| t <= Timestamp(82 * MS)));
    // Still nothing written by then: stuck, and held.
    r.handle_timeout(Timestamp(82 * MS)).unwrap();
    r.poll_actions(&mut out);
    assert!(r.forwarded.contains_key(&id), "held once the disk is stuck");
    assert_eq!(r.metrics.hedged, 0);
}

#[test]
fn a_follower_whose_disk_alone_is_stuck_still_hedges() {
    // Another replica decides a slot after the write was due, before or after δ: other
    // disks work.
    for at in [10, 85] {
        let (mut r, id) = follower_with_a_stuck_disk();
        let mut out = Vec::new();
        let decided = Body::Decided { slot: Slot(9), proposal: proposal(1, 2, 0) };
        r.handle(Timestamp(at * MS), from_peer(2, 1, decided)).unwrap();
        r.poll_actions(&mut out);
        r.handle_timeout(Timestamp(85 * MS)).unwrap();
        r.poll_actions(&mut out);
        assert!(!r.forwarded.contains_key(&id), "decided at {at} ms: hedged after δ");
        assert_eq!(r.metrics.hedged, 1);
    }
}

#[test]
fn a_repeat_of_a_slot_already_applied_is_not_progress_elsewhere() {
    // A compacted slot is no longer in the log, but `learn` ignores it: so must this.
    let (mut r, _) = leader_with_a_stuck_disk();
    let mut out = Vec::new();
    r.emitted = 10;
    let decided = Body::Decided { slot: Slot(5), proposal: proposal(1, 1, 0) };
    r.handle(Timestamp(10 * MS), from_peer(1, 0, decided)).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.watch.disk.last_decided_elsewhere(), Timestamp(0));
}

#[test]
fn an_unknown_persist_id_leaves_the_writes_in_flight_alone() {
    let (mut r, _) = leader_with_a_stuck_disk();
    let before: Vec<u64> = r.watch.disk.in_flight().collect();
    assert!(r.handle(Timestamp(2 * MS), Input::Persisted(PersistId(1 << 40))).is_err());
    assert_eq!(r.watch.disk.in_flight().collect::<Vec<_>>(), before);
}

#[test]
fn a_stepped_aside_deputy_hands_on_what_it_is_handed() {
    // Replica 1 suspects leader 0, so the others hand it their entries; then its disk sticks.
    let mut r = replica_as(1);
    let mut out = Vec::new();
    r.commit_latency = Some(5 * MS);
    r.watch.disk.assume_usual(MS);
    r.watch.suspects.insert(NodeId(0), Timestamp(0));
    r.propose(Timestamp(MS), Bytes::from_static(b"own")).unwrap();
    r.poll_actions(&mut out);
    let decided = Body::Decided { slot: Slot(9), proposal: proposal(1, 2, 0) };
    r.handle(Timestamp(10 * MS), from_peer(2, 1, decided)).unwrap();
    r.poll_actions(&mut out);
    r.handle_timeout(Timestamp(MS + 80 * MS)).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.target(), NodeId(2), "stepped aside");
    out.clear();
    let entries = [batch::Item {
        entry: crate::batch::Entry {
            origin: NodeId(2),
            incarnation: 1,
            id: ProposalId(4),
            payload: Bytes::from_static(b"r"),
        },
        kind: batch::Kind::App,
    }];
    r.handle(r.now, from_peer(2, 1, Body::Forward { batch: batch::encode(&entries) })).unwrap();
    r.poll_actions(&mut out);
    assert!(r.relayed.is_empty(), "not proposed on the stuck disk");
    assert!(!r.batches.values().any(|b| b.entries.iter().any(|e| e.2 == ProposalId(4))));
    assert_eq!(forwards_to(&out, 2), 1, "handed on to the next member");
}

#[test]
fn each_suspicion_is_counted_once() {
    let (r, _) = suspecting();
    assert_eq!(r.metrics().suspicions, 1);
}

#[test]
fn the_usual_persist_latency_moves_by_an_eighth_of_each_sample() {
    let mut r = replica_as(0);
    let mut out = Vec::new();
    r.watch.disk.assume_usual(16 * MS);
    r.propose(Timestamp(MS), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    let last = out
        .iter()
        .filter_map(|a| match a {
            Action::Persist { id, .. } => Some(*id),
            _ => None,
        })
        .max()
        .expect("the write is persisted");
    r.handle(Timestamp(9 * MS), Input::Persisted(last)).unwrap();
    assert_eq!(r.watch.disk.usual(), Some(16 * MS - 2 * MS + MS), "an 8 ms write");
}

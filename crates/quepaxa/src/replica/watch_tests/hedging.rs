//! Hedging around a slow leader: σ, R, the hedge delay, and the late rule.

use super::*;

#[test]
fn an_entry_is_hedged_after_sigma_when_the_leader_says_nothing() {
    let mut r = with_latency(Some(5));
    let mut out = Vec::new();
    r.propose(Timestamp(MS), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(hedge_deadline(&r), Some(Timestamp(MS + 20 * MS)), "σ = max(20 ms, 2 × 5 ms)");
    // Without R, nothing is early.
    let mut plain = with_latency(None);
    plain.propose(Timestamp(MS), Bytes::from_static(b"x")).unwrap();
    plain.poll_actions(&mut out);
    assert_eq!(hedge_deadline(&plain), Some(Timestamp(MS + 80 * MS)), "δ");
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
    assert_eq!(hedge_deadline(&r), Some(Timestamp(MS + 80 * MS)));
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
    assert_eq!(hedge_deadline(&r), Some(at.after(20 * MS)), "silence counts from the forward");
    r.handle(at.after(3 * MS), from_peer(0, 1, Body::ReadQuery { incarnation: 1, round: 1 })).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(hedge_deadline(&r), Some(at.after(80 * MS)), "the leader answered in time");
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
    r.cfg.hedge_min = MS;
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
        r.entries.batches().values().any(|b| b.slot.is_some() && b.entries.iter().any(|e| e.2 == id)),
        "after δ the entry is proposed here"
    );
    out.clear();
    let next = r.propose(r.now, Bytes::from_static(b"y")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 1, "the leader was talking: not suspected");
    assert!(r.entries.forwarded().contains_key(&next));
}

#[test]
fn a_leader_that_keeps_deciding_is_not_hedged_around() {
    let mut r = with_latency(Some(5));
    let mut out = Vec::new();
    // δ = 80 ms; 4δ stays under hedge_max.
    r.cfg.hedge_max = 400 * MS;
    let id = r.propose(Timestamp(MS), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    // The leader took the forward.
    let seqs = forward_seqs(&out, 0);
    r.handle(Timestamp(2 * MS), from_peer(0, 1, Body::ForwardAck { seqs })).unwrap();
    r.poll_actions(&mut out);
    // It decides a slot of its own at once (within σ), then every 100 ms for 1.1 s, long past
    // 4δ from the forward: it is working through a backlog, and a hedge would only add to it.
    for slot in 0..12 {
        let at = Timestamp((4 + slot * 100) * MS);
        r.handle_timeout(at).unwrap();
        r.poll_actions(&mut out);
        assert!(r.entries.forwarded().contains_key(&id), "at {at:?}: hedged around a busy leader");
        leader_decides(&mut r, at, slot);
    }
    // It stops deciding: 4δ after its last decision the entry is hedged, in case the forward
    // was lost.
    let stopped = Timestamp(1_104 * MS);
    assert_eq!(r.next_deadline(), Some(stopped.after(4 * 80 * MS)), "4δ from its last progress");
    r.handle_timeout(stopped.after(4 * 80 * MS - 1)).unwrap();
    r.poll_actions(&mut out);
    assert!(r.entries.forwarded().contains_key(&id), "hedged before 4δ from its last progress");
    r.handle_timeout(stopped.after(4 * 80 * MS)).unwrap();
    r.poll_actions(&mut out);
    assert!(!r.entries.forwarded().contains_key(&id), "a leader that stopped deciding is hedged around");
}

#[test]
fn only_a_new_decision_the_target_announces_counts_as_its_progress() {
    let mut r = with_latency(Some(5));
    let mut out = Vec::new();
    let decided = |slot| Body::decided(Slot(slot), proposal(1, 2, slot));
    // A decision this replica already holds, repeated by the leader.
    announce(&mut r, Timestamp(MS), 2, Slot(0), proposal(1, 2, 0));
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
        let forward = Body::Forward { seq: 0, batch: batch::encode(core::slice::from_ref(e)) };
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
    // Flush any coalesced trailing forward before the leader speaks at 4 ms.
    if let Some(t) = r.earliest_timer()
        && t < Timestamp(4 * MS)
    {
        r.handle_timeout(t).unwrap();
        r.poll_actions(&mut out);
    }
    r.handle(Timestamp(4 * MS), from_peer(0, 1, Body::ReadQuery { incarnation: 1, round: 0 })).unwrap();
    r.poll_actions(&mut out);
    r.handle_timeout(Timestamp(MS + 80 * MS)).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.proposers.len(), 1, "one hedged batch is proposed");
    assert!(!r.entries.relayed().is_empty(), "the rest wait for its slot, though they fill more batches");
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
fn r_counts_from_the_first_forward_the_target_has_not_answered() {
    let mut r = with_latency(Some(5));
    let mut out = Vec::new();
    r.propose(Timestamp(MS), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    r.propose(Timestamp(2 * MS), Bytes::from_static(b"y")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 2);
    r.handle(Timestamp(10 * MS), from_peer(0, 1, Body::ReadQuery { incarnation: 1, round: 0 })).unwrap();
    assert_eq!(r.watch.reply.get(&NodeId(0)), Some(&(9 * MS)), "from 1 ms, not the later forward");
}

#[test]
fn sigma_equal_to_the_hedge_delay_does_not_count() {
    // 2R = δ = 80 ms.
    let r = with_latency(Some(40));
    assert_eq!(r.hedge_delay(), 80 * MS);
    assert_eq!(r.silence_wait(NodeId(0)), None);
}

#[test]
fn progress_announced_at_the_forward_itself_is_not_a_backlog() {
    let mut r = with_latency(None);
    // δ = 80 ms; 4δ stays under hedge_max.
    r.cfg.hedge_max = 400 * MS;
    let at = Timestamp(10 * MS);
    r.watch.progressed = at;
    assert_eq!(r.late_at(at, true), Some(at.after(80 * MS)), "nothing decided after the forward: δ");
    r.watch.progressed = at.after(1);
    assert_eq!(r.late_at(at, true), Some(at.after(1 + 4 * 80 * MS)), "4δ from the progress");
    r.watch.progressed = at.after(500 * MS);
    assert_eq!(r.late_at(at, true), Some(at.after(500 * MS + 4 * 80 * MS)), "not from the forward");
    assert_eq!(r.late_at(at, false), Some(at.after(4 * 80 * MS)), "unacknowledged: from the forward");
    r.watch.progressed = at.after(2_000 * MS);
    assert_eq!(r.late(at, true), Some((at.after(4 * 400 * MS), true)), "at most 4 · hedge_max");
}

#[test]
fn without_silent_leader_takeover_a_silent_leader_is_waited_for_the_hedge_delay() {
    let mut r = with_latency(Some(5));
    r.cfg.silent_leader_takeover = false;
    let mut out = Vec::new();
    r.propose(Timestamp(MS), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(hedge_deadline(&r), Some(Timestamp(MS + 80 * MS)), "δ, as before the option");
    r.handle_timeout(Timestamp(MS + 80 * MS)).unwrap();
    r.poll_actions(&mut out);
    out.clear();
    let next = r.propose(r.now, Bytes::from_static(b"y")).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(forwards_to(&out, 0), 1, "the leader is not suspected");
    assert!(r.entries.forwarded().contains_key(&next));
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
    r.handle(Timestamp(2), from_peer(1, 0, Body::Forward { seq: 0, batch: batch::encode(&entries) }))
        .unwrap();
    r.poll_actions(&mut out);
    let acked = out.iter().any(|a| match a {
        Action::Send(o) => {
            o.to == NodeId(1)
                && matches!(
                    Message::decode(&o.bytes, &r_wire()),
                    Ok(Message { body: Body::ForwardAck { .. }, .. })
                )
        }
        _ => false,
    });
    assert!(acked, "the ack does not wait for the disk: {out:?}");
    // Another forward within an eighth of the retransmit interval goes unacknowledged.
    let acks = |r: &mut Replica, at: u64| {
        let mut out = Vec::new();
        r.handle(Timestamp(at), from_peer(1, 0, Body::Forward { seq: 0, batch: batch::encode(&entries) }))
            .unwrap();
        r.poll_actions(&mut out);
        out.iter()
            .filter(|a| matches!(a, Action::Send(o) if matches!(Message::decode(&o.bytes, &r_wire()), Ok(Message { body: Body::ForwardAck { .. }, .. }))))
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
        && matches!(Message::decode(&o.bytes, &r_wire()), Ok(Message { body: Body::ForwardAck { .. }, .. })))),
        "a suppressed ack is eventually sent without another forward or durable write"
    );
}

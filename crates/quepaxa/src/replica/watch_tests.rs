//! A follower's watch on its leader: suspecting a silent one and proposing directly.

use super::tests::{from_peer, proposal, r_wire, replica_as};
use super::*;

const MS: u64 = 1_000_000;

/// Entries `r` handed to `leader` among `out`.
fn forwards_to(out: &[Action], leader: u32) -> usize {
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
    r.watch.reply = Some(5 * MS);
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
    r.watch.reply = Some(5 * MS);
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
    r.watch.reply = reply_ms.map(|ms| ms * MS);
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
    assert_eq!(r.watch.reply, Some(5 * MS - 5 * MS / 8 + 3 * MS / 8));
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
    assert_eq!(r.silence_wait(), None, "2R past δ: silence says nothing δ does not");
    r.cfg.hedge_max = 400 * MS;
    r.commit_latency = Some(40 * MS);
    assert_eq!(r.silence_wait(), Some(120 * MS), "a leader slow to answer (R = 60 ms) is given 2R");
    r.commit_latency = Some(5 * MS);
    r.watch.reply = Some(MS / 100);
    assert_eq!(r.silence_wait(), Some(10 * MS), "never below the retransmit interval");
    r.cfg.retransmit_interval = MS / 100;
    r.cfg.hedge_max = 2 * MS;
    r.commit_latency = Some(MS);
    assert_eq!(r.silence_wait(), Some(r.cfg.hedge_min), "nor below hedge_min");
    r.watch.reply = None;
    assert_eq!(r.silence_wait(), None);
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
    r.watch.reply = Some(5 * MS);
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
fn r_rises_at_once_to_a_slower_answer() {
    let mut r = with_latency(Some(5));
    let mut out = Vec::new();
    r.propose(Timestamp(MS), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    r.handle(Timestamp(MS + 30 * MS), from_peer(0, 1, Body::ReadQuery { incarnation: 1, round: 0 })).unwrap();
    assert_eq!(r.watch.reply, Some(30 * MS), "a leader whose disk slowed: σ covers it at once");
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
}

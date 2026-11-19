//! A leader stepping aside on a stuck disk, and disk health holding hedges back.

use super::*;

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
    assert!(r.entries.forwarded().contains_key(&id));
    assert!(
        r.entries.own().get(&id).is_some_and(|o| o.hedged),
        "its wait does not count as a commit latency"
    );
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
    assert!(r.entries.relayed().is_empty(), "not hedged here");
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
    assert!(r.entries.forwarded().contains_key(&id), "not hedged past δ");
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
    assert!(!r.entries.forwarded().contains_key(&id), "hedged after δ");
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
    assert!(r.entries.forwarded().contains_key(&id), "held once the disk is stuck");
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
        assert!(!r.entries.forwarded().contains_key(&id), "decided at {at} ms: hedged after δ");
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
    assert!(r.entries.relayed().is_empty(), "not proposed on the stuck disk");
    assert!(!r.entries.batches().values().any(|b| b.entries.iter().any(|e| e.2 == ProposalId(4))));
    assert_eq!(forwards_to(&out, 2), 1, "handed on to the next member");
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

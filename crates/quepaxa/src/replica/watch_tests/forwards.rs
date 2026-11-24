//! Forwards acknowledged by number, sent again when not, and the bound on a hedge (#231).

use super::*;

/// Follower 1 (δ = 80 ms, 4δ = 320 ms, hedge_max = 400 ms; the leader acknowledged earlier
/// forwards within 5 ms, so the resend floor is retransmit_interval, 10 ms) with one entry
/// forwarded to the leader at 1 ms, which the leader then decides around at 4 ms: the
/// entry's id and the forward.
fn forwarded() -> (Replica, ProposalId, (u64, Vec<batch::Item>)) {
    let mut r = with_latency(Some(5));
    let mut out = Vec::new();
    r.cfg.hedge_max = 400 * MS;
    r.watch.fwd.ack_delay.insert(NodeId(0), 5 * MS);
    let id = r.propose(Timestamp(MS), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    let mut sent = forwards_sent(&out, 0);
    assert_eq!(sent.len(), 1);
    leader_decides(&mut r, Timestamp(4 * MS), 0);
    (r, id, sent.remove(0))
}

/// Fires the timers at `at` and keeps the leader deciding there (slot `slot`): the forwards
/// sent to it meanwhile.
fn busy_until(r: &mut Replica, at: Timestamp, slot: u64) -> Vec<(u64, Vec<batch::Item>)> {
    let mut out = Vec::new();
    r.handle_timeout(at).unwrap();
    r.poll_actions(&mut out);
    leader_decides(r, at, slot);
    forwards_sent(&out, 0)
}

#[test]
fn a_forward_lost_on_the_wire_is_sent_again_and_commits_without_a_hedge() {
    let (mut r, id, (seq, items)) = forwarded();
    // The forward never arrives; after the floor it is sent again, same number and entry.
    assert_eq!(r.watch.fwd.due(), Some(Timestamp(11 * MS)), "the resend floor after the forward");
    let again = busy_until(&mut r, Timestamp(11 * MS), 1);
    assert_eq!(again, [(seq, items.clone())]);
    assert_eq!(r.metrics().forward_resends, 1);
    // This copy arrives and is acknowledged.
    r.handle(Timestamp(12 * MS), from_peer(0, 1, Body::ForwardAck { seqs: alloc::vec![seq] })).unwrap();
    r.poll_actions(&mut Vec::new());
    assert_eq!(r.watch.fwd.due(), None, "nothing left to send again");
    // The leader keeps deciding other slots past 4δ from the forward: no hedge.
    for slot in 2..10 {
        let at = Timestamp(slot * 100 * MS);
        assert!(busy_until(&mut r, at, slot).is_empty(), "not sent again once acknowledged");
        assert!(r.entries.forwarded().contains_key(&id), "at {at:?}: hedged around a busy leader");
    }
    // Then it decides the entry itself.
    let value = batch::encode(&items);
    let key = crate::proposal::ValueKey { origin: NodeId(0), incarnation: 1, seq: 99 };
    announce(&mut r, Timestamp(1_000 * MS), 0, Slot(10), Proposal { priority: 1, key, value });
    assert_eq!(r.entries.own_len(), 0, "committed");
    assert_eq!((r.metrics().hedged, r.metrics().forward_resends), (0, 1));
}

#[test]
fn an_unacknowledged_forward_is_hedged_4_delta_after_it_whatever_the_progress() {
    let (mut r, id, (seq, _)) = forwarded();
    // Never acknowledged: resent with backoff (10, 20, 40, 80, 160 ms), and hedged 4δ after
    // the forward although the leader keeps deciding.
    let mut resent = Vec::new();
    for (slot, at) in [11, 31, 71, 151, 311].into_iter().enumerate() {
        resent.extend(busy_until(&mut r, Timestamp(at * MS), slot as u64 + 1).into_iter().map(|(s, _)| s));
    }
    assert_eq!(resent, [seq; 5], "doubling from the floor");
    assert!(r.entries.forwarded().contains_key(&id));
    r.handle_timeout(Timestamp(MS + 4 * 80 * MS)).unwrap();
    r.poll_actions(&mut Vec::new());
    assert!(!r.entries.forwarded().contains_key(&id), "hedged 4δ after the forward");
    assert_eq!(r.metrics().hedges_at_bound, 0);
}

#[test]
fn an_acknowledged_entry_is_hedged_by_4_hedge_max_while_the_target_keeps_deciding() {
    let (mut r, id, (seq, _)) = forwarded();
    r.handle(Timestamp(2 * MS), from_peer(0, 1, Body::ForwardAck { seqs: alloc::vec![seq] })).unwrap();
    r.poll_actions(&mut Vec::new());
    for slot in 1..17 {
        busy_until(&mut r, Timestamp(slot * 100 * MS), slot);
        assert!(r.entries.forwarded().contains_key(&id), "hedged around a busy leader");
    }
    // 4 · hedge_max = 1.6 s after the forward, however busy the leader.
    let bound = Timestamp(MS + 4 * 400 * MS);
    assert_eq!(r.next_deadline(), Some(bound), "not hedged by the hard bound");
    r.handle_timeout(bound).unwrap();
    r.poll_actions(&mut Vec::new());
    assert!(!r.entries.forwarded().contains_key(&id), "not hedged by the hard bound");
    assert_eq!((r.metrics().hedged, r.metrics().hedges_at_bound), (1, 1));
}

#[test]
fn every_forward_taken_is_acknowledged_by_its_number() {
    let mut r = replica_as(0);
    r.cfg.max_outstanding_proposals = 1;
    let item = |id| batch::Item {
        entry: crate::batch::Entry {
            origin: NodeId(1),
            incarnation: 1,
            id: ProposalId(id),
            payload: Bytes::new(),
        },
        kind: batch::Kind::App,
    };
    let acks = |out: &[Action]| -> Vec<Vec<u64>> {
        out.iter()
            .filter_map(|a| match a {
                Action::Send(o) => match Message::decode(&o.bytes, &r_wire()) {
                    Ok(Message { body: Body::ForwardAck { seqs }, .. }) => Some(seqs),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    };
    let mut out = Vec::new();
    let forward = |seq, ids: &[u64]| {
        let items: Vec<_> = ids.iter().map(|id| item(*id)).collect();
        from_peer(1, 0, Body::Forward { seq, batch: batch::encode(&items) })
    };
    r.handle(Timestamp(1), forward(5, &[1])).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(acks(&out), [alloc::vec![5]], "at once");
    out.clear();
    // Within the cooldown: coalesced. One the leader refuses (4 entries over its cap of
    // 1 · 3 members) is not acknowledged.
    r.handle(Timestamp(2), forward(6, &[2])).unwrap();
    r.poll_actions(&mut out);
    r.handle(Timestamp(3), forward(7, &[3, 4, 5, 6])).unwrap();
    r.poll_actions(&mut out);
    assert!(acks(&out).is_empty());
    r.handle_timeout(Timestamp(1).after(r.cfg.retransmit_interval / 8)).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(acks(&out), [alloc::vec![6]], "every forward taken, none refused");
}

#[test]
fn the_forward_resend_floor_follows_the_acknowledgement_delay() {
    let mut r = with_latency(Some(5));
    r.cfg.hedge_max = 400 * MS;
    // R (5 ms here) is not the acknowledgement delay: before one is seen, hedge_max.
    assert_eq!(r.forward_wait(NodeId(0), None), 400 * MS, "no acknowledgement yet: hedge_max");
    let mut out = Vec::new();
    r.propose(Timestamp(MS), Bytes::from_static(b"x")).unwrap();
    r.poll_actions(&mut out);
    let seq = forward_seqs(&out, 0)[0];
    // Acknowledged 30 ms after it was sent, behind a busy link.
    r.handle(Timestamp(31 * MS), from_peer(0, 1, Body::ForwardAck { seqs: alloc::vec![seq] })).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.watch.fwd.ack_delay(NodeId(0)), Some(30 * MS));
    assert_eq!(r.forward_wait(NodeId(0), None), 60 * MS, "twice the acknowledgement delay");
    assert_eq!(r.forward_wait(NodeId(0), Some(1)), 240 * MS, "backed off from there");
    r.watch.fwd.ack_delay.insert(NodeId(0), MS);
    assert_eq!(r.forward_wait(NodeId(0), None), 10 * MS, "never below retransmit_interval");
}

#[test]
fn an_acknowledgement_of_a_resent_forward_only_raises_the_delay() {
    let (mut r, _, (seq, _)) = forwarded();
    // Sent at 1 ms, again at 11 ms; acknowledged at 13 ms. Either copy may be the one
    // answered: 2 ms since the last copy does not pull 5 ms down.
    busy_until(&mut r, Timestamp(11 * MS), 1);
    r.handle(Timestamp(13 * MS), from_peer(0, 1, Body::ForwardAck { seqs: alloc::vec![seq] })).unwrap();
    r.poll_actions(&mut Vec::new());
    assert_eq!(r.watch.fwd.ack_delay(NodeId(0)), Some(5 * MS), "a resent forward's ack is no sample");
    // Another forward, resent at its floor, acknowledged 40 ms after the copy: raised at once.
    let mut out = Vec::new();
    r.propose(Timestamp(20 * MS), Bytes::from_static(b"y")).unwrap();
    r.poll_actions(&mut out);
    let next = forward_seqs(&out, 0)[0];
    busy_until(&mut r, Timestamp(30 * MS), 2);
    r.handle(Timestamp(70 * MS), from_peer(0, 1, Body::ForwardAck { seqs: alloc::vec![next] })).unwrap();
    r.poll_actions(&mut Vec::new());
    assert_eq!(r.watch.fwd.ack_delay(NodeId(0)), Some(40 * MS), "a slower link is followed at once");
}

#[test]
fn resends_count_the_bytes_they_carry() {
    let (mut r, _, (_, items)) = forwarded();
    busy_until(&mut r, Timestamp(11 * MS), 1);
    assert_eq!(r.metrics().forward_resent_bytes, batch::encode(&items).len() as u64);
}

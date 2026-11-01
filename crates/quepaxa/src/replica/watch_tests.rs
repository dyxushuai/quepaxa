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

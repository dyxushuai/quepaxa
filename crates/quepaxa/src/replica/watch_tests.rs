//! A follower's watch on its leader: suspecting a silent one and proposing directly.

use super::disk::Health;
use super::tests::{announce, from_peer, proposal, r_wire, replica_as};
use super::*;

mod forwards;
mod hedging;
mod step_aside;
mod suspicion;

const MS: u64 = 1_000_000;

/// The forwards `r` sent to `to` among `out`: their numbers and entries.
fn forwards_sent(out: &[Action], to: u32) -> Vec<(u64, Vec<batch::Item>)> {
    out.iter()
        .filter_map(|a| match a {
            Action::Send(o) if o.to == NodeId(to) => match Message::decode(&o.bytes, &r_wire()) {
                Ok(Message { body: Body::Forward { seq, batch }, .. }) => {
                    Some((seq, batch::decode(&batch).unwrap()))
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Leader 0 announces `slot` decided with a value of its own at `at`; the record is written.
pub(super) fn leader_decides(r: &mut Replica, at: Timestamp, slot: u64) {
    let mut out = announce(r, at, 0, Slot(slot), proposal(1, 0, slot));
    let last = r.watch.disk.in_flight().next_back();
    if let Some(last) = last {
        r.handle(at, Input::Persisted(PersistId(last))).unwrap();
        r.poll_actions(&mut out);
    }
}

/// When `r` next hedges an entry or suspects its target (not when it sends a forward again).
pub(super) fn hedge_deadline(r: &Replica) -> Option<Timestamp> {
    r.entries.forwarded_since().filter_map(|(id, at)| r.hedge_at(id, at)).min()
}

/// The numbers of the forwards `r` sent to `to` among `out`.
fn forward_seqs(out: &[Action], to: u32) -> Vec<u64> {
    forwards_sent(out, to).into_iter().map(|(seq, _)| seq).collect()
}

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
    while r.entries.forwarded().contains_key(&id) {
        let at = r.next_deadline().expect("the hedge is scheduled");
        r.handle_timeout(at).unwrap();
        r.poll_actions(&mut out);
    }
    (r, id)
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

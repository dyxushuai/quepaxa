//! One test per transition of `Phase`, `CatchUp` and `Reservation`.

use alloc::collections::BTreeSet;

use super::{CatchUp, Phase, Reservation};
use crate::types::{NodeId, Slot, Timestamp};

const ME: NodeId = NodeId(0);
const MEMBERS: [NodeId; 3] = [NodeId(0), NodeId(1), NodeId(2)];

fn recovering() -> Phase {
    let mut p = Phase::start(true, Timestamp(0));
    p.probed(Some(Timestamp(10)));
    p
}

#[test]
fn a_fresh_start_is_live_and_a_restart_recovering() {
    assert_eq!(Phase::start(false, Timestamp(5)), Phase::Live);
    let p = Phase::start(true, Timestamp(5));
    assert_eq!(p, Phase::Recovering { probe_at: Timestamp(5), answered: BTreeSet::new() });
    assert!(!p.fenced());
}

#[test]
fn a_probe_with_someone_to_ask_sets_the_timer() {
    let p = recovering();
    assert_eq!(p, Phase::Recovering { probe_at: Timestamp(10), answered: BTreeSet::new() });
    assert_eq!(p.probe_at(), Some(Timestamp(10)));
}

#[test]
fn a_probe_with_nobody_to_ask_ends_recovery() {
    let mut p = Phase::start(true, Timestamp(0));
    p.probed(None);
    assert_eq!(p, Phase::Live);
    assert_eq!(p.probe_at(), None);
}

#[test]
fn a_live_replica_never_recovers_again() {
    let mut p = Phase::Live;
    p.probed(Some(Timestamp(10)));
    p.answered(NodeId(1));
    p.check(ME, &MEMBERS);
    assert_eq!(p, Phase::Live);
}

#[test]
fn probing_again_keeps_the_answers_and_moves_the_timer() {
    let mut p = recovering();
    p.answered(NodeId(1));
    p.probed(Some(Timestamp(20)));
    assert_eq!(p, Phase::Recovering { probe_at: Timestamp(20), answered: BTreeSet::from([NodeId(1)]) });
}

#[test]
fn an_answer_is_counted_only_while_recovering() {
    let mut p = recovering();
    p.answered(NodeId(2));
    assert_eq!(p, Phase::Recovering { probe_at: Timestamp(10), answered: BTreeSet::from([NodeId(2)]) });
    let mut live = Phase::Live;
    live.answered(NodeId(2));
    assert_eq!(live, Phase::Live);
}

#[test]
fn a_majority_counting_this_replica_ends_recovery() {
    let mut p = recovering();
    p.check(ME, &MEMBERS);
    assert!(matches!(p, Phase::Recovering { .. }), "this replica alone is no majority");
    p.answered(NodeId(1));
    p.check(ME, &MEMBERS);
    assert_eq!(p, Phase::Live);
}

#[test]
fn answers_from_outside_the_members_do_not_count() {
    let mut p = recovering();
    p.answered(NodeId(7));
    p.answered(NodeId(8));
    p.check(ME, &MEMBERS);
    assert!(matches!(p, Phase::Recovering { .. }));
}

#[test]
fn fencing_ends_every_phase_for_good() {
    for mut p in [recovering(), Phase::Live] {
        p.fence();
        assert!(p.fenced());
        p.probed(Some(Timestamp(30)));
        p.answered(NodeId(1));
        p.check(ME, &MEMBERS);
        assert_eq!(p, Phase::Fenced);
        assert_eq!(p.probe_at(), None);
    }
}

#[test]
fn the_most_advanced_peer_is_remembered() {
    let mut c = CatchUp::default();
    c.heard(NodeId(1), 5);
    c.heard(NodeId(2), 3);
    assert!(c.ahead_of(4) && !c.ahead_of(5));
    assert!(c.behind(3) && !c.behind(4), "one slot ahead comes with the traffic");
    c.heard(NodeId(2), 9);
    assert_eq!(c.installed(0), Some(NodeId(2)));
}

#[test]
fn a_request_is_due_again_after_the_interval() {
    let mut c = CatchUp::default();
    assert!(c.due(Timestamp(0), 10));
    c.requested(NodeId(1), Timestamp(5));
    assert!(!c.due(Timestamp(14), 10));
    assert!(c.due(Timestamp(15), 10));
}

#[test]
fn a_page_from_the_peer_asked_asks_it_for_the_next_while_behind() {
    let mut c = CatchUp::default();
    c.heard(NodeId(1), 100);
    c.requested(NodeId(1), Timestamp(5));
    assert_eq!(c.page(NodeId(1), false, 50, Timestamp(6)), Some(NodeId(1)));
    c.requested(NodeId(1), Timestamp(6));
    assert_eq!(c.page(NodeId(1), false, 99, Timestamp(7)), None, "caught up");
    assert!(c.due(Timestamp(7), 10), "nothing in flight");
}

#[test]
fn an_empty_page_leaves_the_request_to_the_timer() {
    let mut c = CatchUp::default();
    c.heard(NodeId(1), 100);
    c.requested(NodeId(1), Timestamp(5));
    assert_eq!(c.page(NodeId(1), true, 50, Timestamp(8)), None);
    assert!(!c.due(Timestamp(17), 10));
    assert!(c.due(Timestamp(18), 10));
}

#[test]
fn a_page_from_another_peer_leaves_the_request_alone() {
    let mut c = CatchUp::default();
    c.heard(NodeId(1), 100);
    c.requested(NodeId(1), Timestamp(5));
    assert_eq!(c.page(NodeId(2), false, 50, Timestamp(6)), None);
    assert!(!c.due(Timestamp(6), 10));
}

#[test]
fn a_snapshot_ends_the_request_and_asks_a_peer_still_ahead() {
    let mut c = CatchUp::default();
    c.heard(NodeId(1), 100);
    c.requested(NodeId(2), Timestamp(5));
    assert_eq!(c.installed(100), None, "the snapshot reached the peer");
    assert!(c.due(Timestamp(5), 10));
    assert_eq!(c.installed(60), Some(NodeId(1)));
}

#[test]
fn the_timer_retries_only_while_behind() {
    let mut c = CatchUp::default();
    assert_eq!(c.deadline(0, Timestamp(3), 10), None);
    c.heard(NodeId(1), 100);
    assert_eq!(c.deadline(98, Timestamp(3), 10), Some(Timestamp(3)));
    assert_eq!(c.deadline(99, Timestamp(3), 10), None);
    c.requested(NodeId(1), Timestamp(5));
    assert_eq!(c.deadline(50, Timestamp(6), 10), Some(Timestamp(15)));
}

#[test]
fn a_retry_prefers_the_most_advanced_peer_and_else_the_read_mark() {
    let mut c = CatchUp::default();
    c.heard(NodeId(1), 100);
    c.requested(NodeId(2), Timestamp(0));
    let read = || Some(NodeId(3));
    assert_eq!(c.retry(50, Timestamp(5), 10, read), None, "not due");
    assert_eq!(c.retry(50, Timestamp(10), 10, read), Some(NodeId(1)));
    assert_eq!(c.retry(99, Timestamp(10), 10, read), Some(NodeId(3)), "caught up with the peers");
    assert_eq!(c.retry(99, Timestamp(10), 10, || None), None);
}

#[test]
fn a_fresh_replica_may_lead_every_slot() {
    let r = Reservation::recovered(None);
    assert!(r.may_lead(Slot(0)));
    assert_eq!(r.checkpoint(), None);
    assert!(!r.covers(Slot(0)));
}

#[test]
fn a_restart_leads_only_past_what_it_reserved_before() {
    let r = Reservation::recovered(Some(40));
    assert!(!r.may_lead(Slot(40)));
    assert!(r.may_lead(Slot(41)));
    assert_eq!(r.checkpoint(), Some(40), "a checkpoint keeps the recovered reservation");
}

#[test]
fn leading_past_the_reservation_extends_it() {
    let mut r = Reservation::recovered(Some(40));
    assert_eq!(r.extend(Slot(41), 16), Some(57));
    assert_eq!(r.extend(Slot(57), 16), None, "still covered");
    assert_eq!(r.extend(Slot(58), 16), Some(74));
    assert_eq!(r.checkpoint(), Some(74));
}

#[test]
fn a_durable_reservation_covers_its_slots() {
    let mut r = Reservation::recovered(None);
    r.extend(Slot(0), 16);
    r.extend(Slot(17), 16);
    assert!(!r.covers(Slot(0)));
    r.made_durable(33);
    r.made_durable(16);
    assert!(r.covers(Slot(33)) && !r.covers(Slot(34)), "an older durable write never lowers it");
}

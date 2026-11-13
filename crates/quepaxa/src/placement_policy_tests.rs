//! The placement state machine's transitions, one by one, and the field combinations that
//! were bugs before the state was explicit.

use super::*;

const PI: u64 = PROBE_INTERVAL;
const SUSTAIN: u64 = 180 * PI;
const HOLD: u64 = 600 * PI;
const PERIODS: Periods = Periods { sustain: SUSTAIN, hold: HOLD };
const WRITES: u64 = 30;
const ORDER: [NodeId; 3] = [NodeId(0), NodeId(1), NodeId(2)];
/// Costs the round trips cannot tell apart.
const TIED: [Option<u128>; 3] = [Some(1_000), Some(1_000), Some(1_000)];

fn t(ns: u64) -> Timestamp {
    Timestamp(ns)
}

/// Samples around `ms` (±1 ms) spanning the sustained period up to `at`.
fn spanning(ms: u64, at: u64) -> Led {
    let mut l = Led::default();
    l.add(ms * 1_000, t(at - SUSTAIN));
    for k in 0..20 {
        l.add(ms * 1_000 + if k % 2 == 0 { 1_000 } else { 0 }, t(at));
    }
    l.add(ms * 1_000 - 1_000, t(at));
    l
}

/// Twenty samples at `ms` over the ten seconds up to `at`.
fn brief(ms: u64, at: u64) -> Led {
    let mut l = Led::default();
    for k in 0..20 {
        l.add(ms * 1_000, t(at - 10 * PI + k * PI / 2));
    }
    l
}

/// Member 0 leading since `since`, its tenure measured at `ms` up to `at`.
fn measured_leader(since: u64, ms: u64, at: u64) -> Policy {
    let mut p = Policy::new(t(since), NodeId(0));
    p.insert_led(NodeId(0), spanning(ms, at));
    p
}

fn decide(p: &mut Policy, now: u64, costs: &[Option<u128>]) -> Option<NodeId> {
    p.decide(t(now), PERIODS, WRITES, Some((&ORDER, costs)))
}

/// Samples a whole sustained tenure under `members[0]`, a round a second up to `until`.
fn lead(p: &mut Policy, from: u64, until: u64, members: &[NodeId], ms: u64) {
    let mut now = from;
    while now <= until {
        p.sample(t(now), HOLD, members, Some(ms * 1_000));
        now += PI;
    }
}

#[test]
fn a_new_policy_is_idle_and_its_tenure_starts_at_the_given_time() {
    let p = Policy::new(t(7 * PI), NodeId(2));
    assert_eq!((p.tenure().leader(), p.tenure().since()), (NodeId(2), t(7 * PI)));
    assert_eq!((p.pending(), p.changed_at()), (Pending::Idle, None));
}

#[test]
fn an_order_change_that_keeps_the_leader_only_restarts_the_hold() {
    let mut p = Policy::new(t(PI), NodeId(0));
    p.insert_led(NodeId(0), spanning(5, SUSTAIN));
    p.on_order_applied(t(SUSTAIN), HOLD, NodeId(0), NodeId(0), false);
    assert_eq!(p.changed_at(), Some(t(SUSTAIN)));
    assert_eq!(p.tenure().since(), t(PI));
    assert!(p.led(NodeId(0)).is_some() && p.exclusion(NodeId(0)).is_none());
}

#[test]
fn a_rotation_demotes_the_old_leader_twice_as_long_each_time() {
    let mut p = Policy::new(t(0), NodeId(0));
    p.on_order_applied(t(PI), HOLD, NodeId(0), NodeId(1), false);
    assert_eq!((p.tenure().leader(), p.tenure().since()), (NodeId(1), t(PI)));
    assert_eq!(p.exclusion(NodeId(0)), Some(Exclusion { until: t(PI + HOLD), factor: 1 }));
    p.on_order_applied(t(2 * PI), HOLD, NodeId(1), NodeId(0), true);
    assert_eq!(p.exclusion(NodeId(1)), None, "a decided change is no demotion");
    p.on_order_applied(t(3 * PI), HOLD, NodeId(0), NodeId(1), false);
    assert_eq!(p.exclusion(NodeId(0)), Some(Exclusion { until: t(3 * PI + 2 * HOLD), factor: 2 }));
}

#[test]
fn a_restart_starts_the_tenure_and_drops_the_target_but_not_the_move() {
    let mut p = Policy::new(t(0), NodeId(0));
    decide(&mut p, PI, &[Some(1_000 * PI as u128), Some(1), None]);
    assert!(p.target().is_some());
    p.on_restart(t(5 * PI), NodeId(1), false);
    assert_eq!((p.tenure().leader(), p.tenure().since()), (NodeId(1), t(5 * PI)));
    assert!(p.target().is_some(), "nothing ever changed: the target stands");
    p.on_restart(t(6 * PI), NodeId(1), true);
    assert_eq!((p.target(), p.changed_at()), (None, Some(t(6 * PI))));
    p.on_submitted(ProposalId(4));
    p.on_restart(t(7 * PI), NodeId(1), true);
    assert_eq!(p.moving(), Some(ProposalId(4)), "a move resolves on its own");
}

#[test]
fn a_round_samples_only_two_rounds_into_a_tenure_and_only_a_reported_tail() {
    let mut p = Policy::new(t(10 * PI), NodeId(0));
    p.sample(t(11 * PI), HOLD, &ORDER, Some(5));
    assert_eq!(p.led(NodeId(0)), None, "the writers report their previous round");
    p.sample(t(12 * PI), HOLD, &ORDER, None);
    assert_eq!(p.led(NodeId(0)), None);
    p.sample(t(12 * PI), HOLD, &ORDER, Some(5));
    assert_eq!(p.led(NodeId(0)).map(|l| (l.count, l.since)), Some((1, t(12 * PI))));
}

#[test]
fn a_round_forgets_departed_members_and_lifts_a_whole_holds_exclusion() {
    let mut p = Policy::new(t(PI), NodeId(0));
    p.insert_exclusion(NodeId(0), Exclusion { until: t(10 * HOLD), factor: 4 });
    p.insert_exclusion(NodeId(1), Exclusion { until: t(10 * HOLD), factor: 1 });
    p.insert_led(NodeId(2), spanning(5, SUSTAIN));
    p.sample(t(PI + HOLD - 1), HOLD, &ORDER[..2], None);
    assert!(p.exclusion(NodeId(0)).is_some(), "not yet a whole hold");
    assert_eq!(p.led(NodeId(2)), None, "2 left");
    p.sample(t(PI + HOLD), HOLD, &ORDER[..2], None);
    assert_eq!(p.exclusion(NodeId(0)), None);
    assert!(p.exclusion(NodeId(1)).is_some(), "only the leader's");
}

#[test]
fn a_move_is_pending_from_submission_until_it_resolves() {
    let mut p = Policy::new(t(0), NodeId(0));
    p.on_submitted(ProposalId(3));
    assert_eq!(p.moving(), Some(ProposalId(3)));
    assert!(!p.on_resolved(ProposalId(2)), "another proposal");
    assert_eq!(p.moving(), Some(ProposalId(3)));
    assert!(p.on_resolved(ProposalId(3)));
    assert_eq!(p.pending(), Pending::Idle);
    assert!(!p.on_resolved(ProposalId(3)), "resolved once");
}

#[test]
fn nothing_is_decided_while_a_move_is_pending() {
    let mut p = measured_leader(0, 5, SUSTAIN);
    p.on_submitted(ProposalId(1));
    let gain = [Some(1_000 * PI as u128), Some(1), None];
    for k in 1..=SUSTAIN / PI + 1 {
        assert_eq!(decide(&mut p, k * PI, &gain), None);
    }
    assert_eq!(decide(&mut p, SUSTAIN + PI, &TIED), None);
    assert_eq!(p.moving(), Some(ProposalId(1)));
}

#[test]
fn a_gain_is_tracked_and_moves_after_the_sustained_period() {
    let mut p = Policy::new(t(0), NodeId(0));
    let gain = [Some(1_000 * PI as u128), Some(1), None];
    let mut now = PI;
    while now < PI + SUSTAIN {
        assert_eq!(decide(&mut p, now, &gain), None);
        now += PI;
    }
    assert_eq!(p.target(), Some(Target { member: NodeId(1), leader: NodeId(0), since: t(PI) }));
    assert_eq!(decide(&mut p, now, &gain), Some(NodeId(1)));
    assert!(p.target().is_some(), "tracked until the move is submitted");
    p.on_submitted(ProposalId(0));
    assert_eq!(p.target(), None);
}

#[test]
fn a_round_too_late_or_not_ranked_drops_the_target() {
    let gain = [Some(1_000 * PI as u128), Some(1), None];
    let mut p = Policy::new(t(0), NodeId(0));
    decide(&mut p, PI, &gain);
    assert_eq!(p.decide(t(2 * PI), PERIODS, WRITES, None), None);
    assert_eq!(p.target(), None, "this replica may not move");
    decide(&mut p, 3 * PI, &gain);
    assert_eq!(p.decide(t(4 * PI), PERIODS, WRITE_FLOOR - 1, Some((&ORDER, &gain))), None);
    assert_eq!(p.target(), None, "too few writes");
    decide(&mut p, 5 * PI, &gain);
    decide(&mut p, 7 * PI + 1, &gain);
    assert_eq!(p.target().map(|t| t.since), Some(t(7 * PI + 1)), "rounds too far apart start over");
}

// The bugs found while landing leader measurement (#194), each a field combination no rule
// meant, now ruled out by the state's shape.

#[test]
fn a_member_measured_once_is_not_tried_again_within_the_stale_window() {
    // Endless trials. 0 explores 1; 1 leads a whole tenure and hands the lead back. 1's
    // measurement stays: only leading again starts it over.
    let mut p = measured_leader(0, 5, SUSTAIN);
    assert_eq!(decide(&mut p, SUSTAIN, &TIED), Some(NodeId(1)));
    p.on_submitted(ProposalId(0));
    p.on_resolved(ProposalId(0));
    p.on_order_applied(t(SUSTAIN + PI), HOLD, NodeId(0), NodeId(1), true);
    let back = 2 * SUSTAIN + 3 * PI;
    lead(&mut p, SUSTAIN + 3 * PI, back, &[NodeId(1), NodeId(0), NodeId(2)], 5);
    p.on_order_applied(t(back), HOLD, NodeId(1), NodeId(0), true);
    p.insert_led(NodeId(0), spanning(5, 2 * back));
    assert_eq!(decide(&mut p, 2 * back, &TIED), Some(NodeId(2)), "1 is measured, 2 is next");
    let stale = back + STALE_HOLDS * HOLD;
    p.insert_led(NodeId(2), spanning(5, 2 * back));
    p.insert_led(NodeId(0), spanning(5, stale));
    assert_eq!(decide(&mut p, stale, &TIED), Some(NodeId(1)), "until its measurement goes stale");
}

#[test]
fn a_member_rotation_demoted_is_not_tried_within_four_holds() {
    // 1 led, was rotated out, and 0 leads a measured tenure since.
    let mut p = Policy::new(t(0), NodeId(1));
    p.on_order_applied(t(PI), HOLD, NodeId(1), NodeId(0), false);
    let until = PI + HOLD;
    let now = until + 4 * HOLD - 1;
    p.insert_led(NodeId(0), spanning(5, now));
    assert_eq!(decide(&mut p, now, &TIED), Some(NodeId(2)), "1 was demoted too recently");
    p.insert_led(NodeId(2), spanning(5, now));
    assert_eq!(decide(&mut p, now, &TIED), None);
    p.insert_led(NodeId(0), spanning(5, now + 1));
    assert_eq!(decide(&mut p, now + 1, &TIED), Some(NodeId(1)));
}

#[test]
fn a_tenure_starts_at_the_replicas_clock_not_at_zero() {
    // A host clock far past the sustained period: the tenure still has to last one.
    let start = 1_000 * SUSTAIN;
    for mut p in [Policy::new(t(start), NodeId(0)), {
        let mut p = Policy::new(t(0), NodeId(0));
        p.on_restart(t(start), NodeId(0), false);
        p
    }] {
        p.insert_led(NodeId(0), spanning(5, start + PI));
        assert_eq!(decide(&mut p, start + PI, &TIED), None);
        assert_eq!(decide(&mut p, start + SUSTAIN, &TIED), Some(NodeId(1)));
    }
}

#[test]
fn samples_from_before_a_tenure_never_count_toward_it() {
    // A stale tail counted under a new leader: 1's samples from an earlier tenure are gone
    // once it leads again, and its first two rounds sample nothing.
    let mut p = Policy::new(t(0), NodeId(0));
    p.insert_led(NodeId(1), spanning(1, SUSTAIN));
    p.on_order_applied(t(SUSTAIN), HOLD, NodeId(0), NodeId(1), true);
    assert_eq!(p.led(NodeId(1)), None);
    let by_one = [NodeId(1), NodeId(0), NodeId(2)];
    p.sample(t(SUSTAIN + PI), HOLD, &by_one, Some(9_000));
    assert_eq!(p.led(NodeId(1)), None);
    // Nor after a snapshot or restart, which may also change who leads.
    p.insert_led(NodeId(2), spanning(1, SUSTAIN));
    p.on_restart(t(SUSTAIN + 2 * PI), NodeId(2), true);
    assert_eq!(p.led(NodeId(2)), None);
    // A round under a leader this policy did not see start begins its tenure.
    p.sample(t(SUSTAIN + 3 * PI), HOLD, &by_one, Some(9_000));
    assert_eq!((p.tenure().leader(), p.tenure().since()), (NodeId(1), t(SUSTAIN + 3 * PI)));
    assert_eq!(p.led(NodeId(1)), None);
}

#[test]
fn an_idle_stretch_of_a_tenure_is_not_measured() {
    // Leading since zero, but samples (writes) began ten seconds ago.
    let now = 2 * SUSTAIN;
    let mut p = Policy::new(t(0), NodeId(0));
    p.insert_led(NodeId(0), brief(5, now));
    assert!(!p.led(NodeId(0)).unwrap().spans(SUSTAIN));
    assert_eq!(decide(&mut p, now, &TIED), None);
}

#[test]
fn a_tenure_cut_short_is_explored_not_exploited() {
    // 1 led for ten seconds and looked fast; 2 is measured, slower than 0 is not.
    let mut p = measured_leader(0, 9, SUSTAIN);
    p.insert_led(NodeId(1), brief(1, SUSTAIN));
    p.insert_led(NodeId(2), spanning(7, SUSTAIN));
    p.on_order_applied(t(SUSTAIN), HOLD, NodeId(0), NodeId(0), true);
    assert_eq!(decide(&mut p, SUSTAIN, &TIED), Some(NodeId(1)), "explored, though within a hold");
    let mut p = measured_leader(0, 9, SUSTAIN);
    p.insert_led(NodeId(1), spanning(5, SUSTAIN));
    p.insert_led(NodeId(2), spanning(7, SUSTAIN));
    assert_eq!(decide(&mut p, SUSTAIN, &TIED), Some(NodeId(1)), "measured: the fastest leads");
    p.on_order_applied(t(SUSTAIN), HOLD, NodeId(0), NodeId(0), true);
    assert_eq!(decide(&mut p, SUSTAIN, &TIED), None, "but not within a hold");
}

#[test]
fn the_leader_s_own_measurement_follows_the_same_rule() {
    // Spans the period but stale: the leader does not hand the lead over on it.
    let now = SUSTAIN + STALE_HOLDS * HOLD;
    let mut p = measured_leader(0, 5, SUSTAIN);
    assert_eq!(decide(&mut p, now, &TIED), None);
    p.insert_led(NodeId(0), spanning(5, now));
    assert_eq!(decide(&mut p, now, &TIED), Some(NodeId(1)));
}

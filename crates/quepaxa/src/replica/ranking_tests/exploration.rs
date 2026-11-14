//! Exploring members the round trips cannot tell apart, and measuring tenures.

use super::*;

#[test]
fn a_measured_leader_explores_a_member_the_round_trips_cannot_tell_apart() {
    let mut r = explored(5, &[]);
    // A recent order change does not hold exploration back: each tenure took a sustained
    // period already.
    r.placement.policy.on_order_applied(Timestamp(SUSTAIN), HOLD, NodeId(0), NodeId(0), true);
    round(&mut r, SUSTAIN + PROBE_INTERVAL, &rows(&LAN), &ALL);
    assert_eq!(first(&r), Some(1));
}

#[test]
fn no_exploration_before_the_tenure_is_measured() {
    let mut r = leader(0, &[0, 1, 2]);
    led(&mut r, 0, 5, 5, SUSTAIN);
    round(&mut r, SUSTAIN + PROBE_INTERVAL, &rows(&LAN), &ALL);
    assert_eq!(first(&r), None, "too few rounds");
    let mut r = leader(0, &[0, 1, 2]);
    r.placement.policy.on_restart(Timestamp(PROBE_INTERVAL), NodeId(0), false);
    led(&mut r, 0, 5, 20, SUSTAIN);
    round(&mut r, SUSTAIN, &rows(&LAN), &ALL);
    assert_eq!(first(&r), None, "not a sustained period");
}

#[test]
fn members_the_round_trips_rank_apart_are_not_explored() {
    // 1 and 2 are 200 ms apart: each costs 20% more per write than leader 0.
    let mut r = explored(5, &[]);
    round(&mut r, SUSTAIN + PROBE_INTERVAL, &rows(&three(200)), &ALL);
    assert_eq!(first(&r), None);
}

#[test]
fn a_measured_member_is_skipped_until_its_measurement_goes_stale() {
    let mut r = explored(5, &[(1, 5, SUSTAIN)]);
    round(&mut r, SUSTAIN + PROBE_INTERVAL, &rows(&LAN), &ALL);
    assert_eq!(first(&r), Some(2), "1 is measured");
    // Every member measured, alike: nothing moves.
    let mut r = explored(5, &[(1, 5, SUSTAIN), (2, 5, SUSTAIN)]);
    round(&mut r, SUSTAIN + PROBE_INTERVAL, &rows(&LAN), &ALL);
    assert_eq!(first(&r), None);
    // 1's measurement went stale: it is measured again.
    let t = 9 * HOLD;
    let mut r = leader(0, &[0, 1, 2]);
    led(&mut r, 0, 5, 20, t);
    led(&mut r, 1, 5, 20, SUSTAIN);
    led(&mut r, 2, 5, 20, t);
    round(&mut r, t, &rows(&LAN), &ALL);
    assert_eq!(first(&r), Some(1));
}

#[test]
fn a_tenure_that_began_idle_explores_only_once_its_writes_span_the_period() {
    // Leading since time zero, but writes (and samples) began ten seconds ago.
    let t = 2 * SUSTAIN;
    let mut r = leader(0, &[0, 1, 2]);
    let mut l = Led::default();
    for k in 0..20 {
        l.add(5_000, Timestamp(t - 10 * PROBE_INTERVAL + k * PROBE_INTERVAL / 2));
    }
    r.placement.policy.insert_led(NodeId(0), l);
    round(&mut r, t, &rows(&LAN), &ALL);
    assert_eq!(first(&r), None);
}

#[test]
fn a_tenure_cut_short_is_not_a_measurement() {
    // 1 led for ten seconds only, and looked fast; 2 is measured. 1 is explored, not
    // exploited.
    let mut r = explored(9, &[(2, 7, SUSTAIN)]);
    let mut l = Led::default();
    for k in 0..20 {
        l.add(1_000, Timestamp(SUSTAIN - 10 * PROBE_INTERVAL + k * PROBE_INTERVAL / 2));
    }
    r.placement.policy.insert_led(NodeId(1), l);
    r.placement.policy.on_order_applied(Timestamp(SUSTAIN), HOLD, NodeId(0), NodeId(0), true);
    round(&mut r, SUSTAIN + PROBE_INTERVAL, &rows(&LAN), &ALL);
    assert_eq!(first(&r), Some(1), "explored, though within a hold");
}

#[test]
fn once_every_tied_member_is_measured_the_fastest_leads() {
    let mut r = explored(9, &[(1, 5, SUSTAIN), (2, 7, SUSTAIN)]);
    round(&mut r, SUSTAIN + PROBE_INTERVAL, &rows(&LAN), &ALL);
    assert_eq!(first(&r), Some(1));
    // Not within a hold of the last change.
    let mut r = explored(9, &[(1, 5, SUSTAIN), (2, 7, SUSTAIN)]);
    r.placement.policy.on_order_applied(Timestamp(SUSTAIN), HOLD, NodeId(0), NodeId(0), true);
    round(&mut r, SUSTAIN + PROBE_INTERVAL, &rows(&LAN), &ALL);
    assert_eq!(first(&r), None);
}

#[test]
fn a_member_faster_by_less_than_a_tenth_is_not_worth_a_move() {
    let mut r = explored(11, &[(1, 10, SUSTAIN), (2, 11, SUSTAIN)]);
    round(&mut r, SUSTAIN + PROBE_INTERVAL, &rows(&LAN), &ALL);
    assert_eq!(first(&r), None);
    let mut r = explored(12, &[(1, 10, SUSTAIN), (2, 12, SUSTAIN)]);
    round(&mut r, SUSTAIN + PROBE_INTERVAL, &rows(&LAN), &ALL);
    assert_eq!(first(&r), Some(1));
}

#[test]
fn a_member_demoted_within_four_holds_is_not_explored() {
    // 1 was demoted by rotation; its exclusion ended a hold ago.
    let t = 5 * HOLD;
    let mut r = leader(0, &[0, 1, 2]);
    led(&mut r, 0, 5, 20, t);
    r.placement.policy.insert_exclusion(NodeId(1), Exclusion { until: Timestamp(t - HOLD), factor: 1 });
    round(&mut r, t, &rows(&LAN), &ALL);
    assert_eq!(first(&r), Some(2), "1 was demoted too recently");
    // Four holds after its exclusion ended, it may be explored again.
    let mut r = leader(0, &[0, 1, 2]);
    led(&mut r, 0, 5, 20, t);
    r.placement.policy.insert_exclusion(NodeId(1), Exclusion { until: Timestamp(0), factor: 1 });
    round(&mut r, t, &rows(&LAN), &ALL);
    assert_eq!(first(&r), Some(1));
}

#[test]
fn round_trips_within_a_datacenter_never_move_the_leader() {
    // Only 1 writes; round trips 0-1 and 1-2 are 0.2 ms, 0-2 is 0.8 ms (`round` takes whole
    // ms). 1 costs half of what 0 does, but 0.2 ms a write less: below the slack.
    let mut r = leader(0, &[0, 1, 2]);
    let mut now = PROBE_INTERVAL;
    for _ in 0..2 * SUSTAIN / PROBE_INTERVAL {
        let at = Timestamp(now);
        r.placement.rtt.insert(NodeId(1), (200_000, at));
        r.placement.rtt.insert(NodeId(2), (800_000, at));
        r.placement.tables.insert(NodeId(1), (alloc::vec![(NodeId(0), 200_000), (NodeId(2), 200_000)], at));
        r.placement.tables.insert(NodeId(2), (alloc::vec![(NodeId(0), 800_000), (NodeId(1), 200_000)], at));
        r.placement.writes = [(NodeId(1), 10)].into_iter().collect();
        r.placement.last_write = Some(at);
        r.placement.next_probe = Some(at);
        r.handle_timeout(at).unwrap();
        let mut out = Vec::new();
        r.poll_actions(&mut out);
        now += PROBE_INTERVAL;
    }
    assert_eq!(moved(&r), None);
}

#[test]
fn the_writers_tail_is_sampled_under_the_leader_in_force() {
    let mut r = leader(1, &[0, 1, 2]);
    r.note_commit_latency(4 * MS);
    r.placement.observed.insert(NodeId(0), (6 * MS, Timestamp(3 * PROBE_INTERVAL)));
    r.placement.observed.insert(NodeId(2), (8 * MS, Timestamp(3 * PROBE_INTERVAL)));
    round(&mut r, 3 * PROBE_INTERVAL, &rows(&LAN), &ALL);
    let l = r.placement.policy.led(NodeId(0)).unwrap();
    assert_eq!((l.count, l.sum), (1, 6_000));
}

#[test]
fn the_writers_tail_needs_writers_with_half_the_writes() {
    // Only this replica (1) reports a tail: exactly half the writes is enough.
    let mut r = leader(1, &[0, 1, 2]);
    r.note_commit_latency(4 * MS);
    round(&mut r, 3 * PROBE_INTERVAL, &rows(&LAN), &[(0, 8), (1, 8)]);
    let l = r.placement.policy.led(NodeId(0)).map(|l| (l.count, l.sum));
    assert_eq!(l, Some((1, 4_000)));
    let mut r = leader(1, &[0, 1, 2]);
    r.note_commit_latency(4 * MS);
    round(&mut r, 3 * PROBE_INTERVAL, &rows(&LAN), &[(0, 9), (1, 8)]);
    assert_eq!(r.placement.policy.led(NodeId(0)), None, "less than half the writes");
}

#[test]
fn an_application_write_on_its_way_holds_nothing_back() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    r.entries.insert_own(ProposalId(99), Timestamp(0), Kind::App);
    rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL + 1, &rows(&three(50)), &writes);
    assert_eq!(first(&r), Some(1));
}

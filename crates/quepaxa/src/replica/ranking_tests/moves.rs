//! Ranking by round trips and write counts, and when the leader moves.

use super::*;

#[test]
fn the_leader_moves_to_the_best_placed_member_after_the_sustained_period() {
    // ap-ne leads; everyone writes equally: us-east is best placed.
    let mut r = leader(3, &[3, 0, 1, 2, 4]);
    let all = [(0, 10), (1, 10), (2, 10), (3, 10), (4, 10)];
    let t = rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL, &wan(), &all);
    assert_eq!(moved(&r), None, "not before the sustained period");
    round(&mut r, t, &wan(), &all);
    assert_eq!(moved(&r), Some(alloc::vec![0, 1, 2, 4, 3]));
    // One move at a time.
    let id = r.placement.policy.moving();
    rounds(&mut r, t + PROBE_INTERVAL, 5, &wan(), &all);
    assert_eq!(r.placement.policy.moving(), id);
    assert_eq!(r.entries.own().values().filter(|o| o.kind == Kind::Reconfigure).count(), 1);
}

#[test]
fn a_gain_below_twenty_percent_never_moves_the_leader() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    rounds(&mut r, PROBE_INTERVAL, 2 * SUSTAIN / PROBE_INTERVAL, &rows(&three(72)), &writes);
    assert_eq!(moved(&r), None);
    let mut r = leader(0, &[0, 1, 2]);
    rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL + 1, &rows(&three(70)), &writes);
    assert_eq!(moved(&r), Some(alloc::vec![1, 0, 2]));
}

#[test]
fn a_round_without_the_gain_restarts_the_period() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    let t = rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL - 10, &rows(&three(50)), &writes);
    round(&mut r, t, &rows(&three(100)), &writes);
    let t = rounds(&mut r, t + PROBE_INTERVAL, 20, &rows(&three(50)), &writes);
    assert_eq!(moved(&r), None, "the period started over");
    rounds(&mut r, t, SUSTAIN / PROBE_INTERVAL, &rows(&three(50)), &writes);
    assert!(moved(&r).is_some());
}

#[test]
fn bursts_separated_by_silence_never_add_up() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    let half = SUSTAIN / PROBE_INTERVAL / 2 + 5;
    let t = rounds(&mut r, PROBE_INTERVAL, half, &rows(&three(50)), &writes);
    rounds(&mut r, t + 60 * PROBE_INTERVAL, half, &rows(&three(50)), &writes);
    assert_eq!(moved(&r), None);
}

#[test]
fn too_few_writes_move_nothing() {
    let mut r = leader(0, &[0, 1, 2]);
    rounds(&mut r, PROBE_INTERVAL, 2 * SUSTAIN / PROBE_INTERVAL, &rows(&three(50)), &[(1, 0)]);
    assert_eq!(moved(&r), None);
}

#[test]
fn a_reorder_behind_the_leader_proposes_nothing() {
    // The leader is already best placed; only the others would swap.
    let writes = [(0, 10)];
    let mut r = leader(0, &[0, 2, 1]);
    rounds(
        &mut r,
        PROBE_INTERVAL,
        2 * SUSTAIN / PROBE_INTERVAL,
        &rows(&[[0, 50, 100], [50, 0, 70], [100, 70, 0]]),
        &writes,
    );
    assert_eq!(moved(&r), None);
}

#[test]
fn a_candidate_a_writer_cannot_reach_is_skipped() {
    // us-east is best placed, but neither it nor sa-east (writing) reports a round trip
    // between them: us-west is placed instead.
    let mut r = leader(3, &[3, 0, 1, 2, 4]);
    let mut m = WAN;
    (m[4][0], m[0][4]) = (UNKNOWN, UNKNOWN);
    let m: Vec<&[u64]> = m.iter().map(|r| &r[..]).collect();
    let all = [(0, 10), (1, 10), (2, 10), (3, 10), (4, 10)];
    rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL + 1, &m, &all);
    assert_eq!(moved(&r).map(|o| o[0]), Some(1));
}

#[test]
fn without_a_majority_of_round_trips_nothing_moves() {
    // Nobody reports a round trip to the leader: it has no cost, and nothing is proposed.
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(1, &[1, 0, 2]);
    let m = [[0, UNKNOWN, 50], [UNKNOWN, 0, UNKNOWN], [50, UNKNOWN, 0]];
    rounds(&mut r, PROBE_INTERVAL, 2 * SUSTAIN / PROBE_INTERVAL, &rows(&m), &writes);
    assert_eq!(moved(&r), None);
}

#[test]
fn only_the_leader_in_force_evaluates() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(1, &[0, 1, 2]);
    rounds(&mut r, PROBE_INTERVAL, 2 * SUSTAIN / PROBE_INTERVAL, &rows(&three(50)), &writes);
    assert_eq!(moved(&r), None);
    // Stepped aside when the move would be due.
    let mut r = leader(0, &[0, 1, 2]);
    let t = rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL, &rows(&three(50)), &writes);
    assert!(r.placement.policy.target().is_some());
    r.now = Timestamp(t);
    r.watch.aside = true;
    r.evaluate_placement();
    assert_eq!((moved(&r), r.placement.policy.target()), (None, None));
}

#[test]
fn an_excluded_member_is_not_placed_first() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    let until = Timestamp(10 * HOLD);
    r.placement.policy.insert_exclusion(NodeId(1), Exclusion { until, factor: 1 });
    rounds(&mut r, PROBE_INTERVAL, 2 * SUSTAIN / PROBE_INTERVAL, &rows(&three(50)), &writes);
    assert_eq!(moved(&r), None);
}

#[test]
fn a_writer_that_stopped_decays_to_nothing() {
    let mut r = leader(0, &[0, 1, 2]);
    let t = rounds(&mut r, PROBE_INTERVAL, 20, &rows(&three(50)), &[(1, 10), (2, 1)]);
    assert!(r.placement.rates.contains_key(&NodeId(2)));
    rounds(&mut r, t, 60, &rows(&three(50)), &[(1, 10)]);
    assert!(!r.placement.rates.contains_key(&NodeId(2)), "{:?}", r.placement.rates);
}

#[test]
fn the_best_member_is_placed_even_if_another_started_the_period() {
    // us-east sends no table at first, so it cannot be ranked and us-west starts the period;
    // once us-east is rankable it ranks best, and it is the one placed.
    let mut r = leader(3, &[3, 0, 1, 2, 4]);
    let all = [(0, 10), (1, 10), (2, 10), (3, 10), (4, 10)];
    let mut silent = WAN;
    silent[0] = [0, UNKNOWN, UNKNOWN, UNKNOWN, UNKNOWN];
    let silent: Vec<&[u64]> = silent.iter().map(|r| &r[..]).collect();
    let t = rounds(&mut r, PROBE_INTERVAL, 5, &silent, &all);
    assert_eq!(r.placement.policy.target().map(|t| t.member), Some(NodeId(1)));
    rounds(&mut r, t, SUSTAIN / PROBE_INTERVAL, &wan(), &all);
    assert_eq!(moved(&r).map(|o| o[0]), Some(0));
}

#[test]
fn one_write_a_round_reaches_the_floor() {
    let mut r = leader(0, &[0, 1, 2]);
    rounds(&mut r, PROBE_INTERVAL, 60, &rows(&three(50)), &[(1, 1)]);
    assert_eq!(r.placement.rates[&NodeId(1)], 8);
}

#[test]
fn the_member_view_reports_what_each_peer_said_last() {
    let mut r = leader(0, &[0, 1, 2]);
    round(&mut r, PROBE_INTERVAL, &rows(&three(50)), &[(0, 1)]);
    let env =
        Envelope { cluster_id: [3; 16], config_epoch: 0, from: NodeId(1), to: NodeId(0), commit_index: 9 };
    let msg = Message { env, body: Body::CatchUp { from: Slot(0) } }.encode();
    r.handle(Timestamp(2 * PROBE_INTERVAL), Input::Message(msg)).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    let view = r.members();
    assert_eq!(
        view[0],
        MemberView { id: NodeId(0), round_trip: None, committed: Some(r.committed), last_heard: None }
    );
    assert_eq!(view[1].committed, Some(9));
    assert_eq!(view[1].last_heard, Some(Timestamp(2 * PROBE_INTERVAL)));
    assert_eq!(view[1].round_trip, Some(100 * MS));
    assert_eq!(view[2].committed, None);
}

#[test]
fn a_member_whose_table_went_stale_is_not_placed_first() {
    // us-east reported once, then went silent: its table ages out, so us-west is placed.
    let mut r = leader(3, &[3, 0, 1, 2, 4]);
    let all = [(0, 10), (1, 10), (2, 10), (3, 10), (4, 10)];
    let t = rounds(&mut r, PROBE_INTERVAL, 2, &wan(), &all);
    let mut silent = WAN;
    silent[0] = [0, UNKNOWN, UNKNOWN, UNKNOWN, UNKNOWN];
    let silent: Vec<&[u64]> = silent.iter().map(|r| &r[..]).collect();
    rounds(&mut r, t, SUSTAIN / PROBE_INTERVAL + 10, &silent, &all);
    assert_eq!(moved(&r).map(|o| o[0]), Some(1), "a stale member is never placed first");
}

#[test]
fn round_trips_to_just_a_majority_rank_a_candidate() {
    // Member 1 knows no round trip to 2: with itself and 0 it still has a majority.
    let mut r = leader(0, &[0, 1, 2]);
    let m = [[0, 100, 100], [100, 0, UNKNOWN], [100, UNKNOWN, 0]];
    rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL + 1, &rows(&m), &[(1, 10)]);
    assert_eq!(moved(&r), Some(alloc::vec![1, 0, 2]));
}

#[test]
fn rounds_two_probe_intervals_apart_still_add_up() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    let mut t = PROBE_INTERVAL;
    for _ in 0..=SUSTAIN / (2 * PROBE_INTERVAL) {
        round(&mut r, t, &rows(&three(50)), &writes);
        t += 2 * PROBE_INTERVAL;
    }
    assert_eq!(moved(&r).map(|o| o[0]), Some(1));
}

#[test]
fn writes_exactly_at_the_floor_move_the_leader() {
    // One write a round converges to a rate of exactly the floor.
    let mut r = leader(0, &[0, 1, 2]);
    rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL + 20, &rows(&three(50)), &[(1, 1)]);
    assert_eq!(r.placement.rates.values().sum::<u64>(), 8);
    assert_eq!(moved(&r).map(|o| o[0]), Some(1));
}

#[test]
fn an_exclusion_ends_at_its_deadline() {
    // Member 1 is excluded until the first round only: the period starts with it.
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    let until = Timestamp(PROBE_INTERVAL);
    r.placement.policy.insert_exclusion(NodeId(1), Exclusion { until, factor: 1 });
    rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL + 1, &rows(&three(50)), &writes);
    assert_eq!(moved(&r).map(|o| o[0]), Some(1));
}

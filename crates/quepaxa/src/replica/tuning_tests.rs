//! Leader tuning decisions, from injected epoch reports and phases.

use super::*;
use crate::codec::LatencyStat;
use crate::schedule::{LAG, Phase, Tag, TuningState};

const E: u32 = 128;

/// Replica `me` of `order` (which leads in that order), tuning on with `min` samples.
fn replica(me: u32, order: &[u32], min: u32) -> Replica {
    let members: Vec<NodeId> = order.iter().copied().map(NodeId).collect();
    let mut cfg = Config::new([3; 16], NodeId(me), members.clone(), [9; 32]);
    cfg.leader_order = members;
    cfg.tuning_min_samples = min;
    let mut r = Replica::new(cfg, [], Timestamp(0)).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    r.handle(Timestamp(0), Input::Persisted(PersistId(0))).unwrap();
    r.poll_actions(&mut out);
    r
}

fn stat(leader: u32, count: u64, ms: u64) -> LatencyStat {
    let mut s = LatencyStat::new(NodeId(leader));
    for _ in 0..count {
        s.add(ms * 1_000);
    }
    s
}

/// `stat`, with every sample spread by ±`spread_ms` around `ms`.
fn spread(leader: u32, count: u64, ms: u64, spread_ms: u64) -> LatencyStat {
    let mut s = LatencyStat::new(NodeId(leader));
    for i in 0..count {
        s.add(if i % 2 == 0 { (ms + spread_ms) * 1_000 } else { (ms - spread_ms) * 1_000 });
    }
    s
}

fn report(r: &mut Replica, from: u32, first: u64, stats: &[LatencyStat]) {
    r.on_epoch_report(NodeId(from), Slot(first), None, false, stats.to_vec());
}

/// The change tuning submitted, if any: its order and phase.
fn proposed(r: &Replica) -> Option<(Vec<u32>, Phase)> {
    let id = r.tuning.moving?;
    let item =
        r.pending.iter().find(|i| i.entry.id == id).map(|i| i.entry.payload.clone()).or_else(|| {
            r.batches.values().flat_map(|b| batch::changes(&b.value)).find(|(k, _)| k.2 == id).map(|(_, p)| p)
        })?;
    let (_, order, tag) = crate::schedule::decode_change(&item)?;
    Some((order.into_iter().map(|n| n.0).collect(), tag.phase))
}

/// Forgets the submitted change so the next decision can be checked.
fn clear(r: &mut Replica) {
    if let Some(id) = r.tuning.moving.take() {
        r.outstanding.remove(&id);
        r.pending.retain(|i| i.entry.id != id);
        r.batches.retain(|_, b| batch::changes(&b.value).iter().all(|(k, _)| k.2 != id));
    }
}

fn phase(r: &mut Replica, phase: Phase) {
    r.applied_tuning.tag = Tag { phase, epoch_slots: E };
}

#[test]
fn rewards_are_weighted_by_entry_count() {
    let mut r = replica(0, &[0, 1, 2, 3], 1);
    for from in 1..4 {
        report(&mut r, from, 128, &[stat(0, 10, 4)]);
    }
    r.store_report(NodeId(0), 128, None, false, alloc::vec![stat(0, 30, 8)]);
    let (s, arm) = r.epoch_stat(128, NodeId(0));
    assert_eq!((s.count, s.sum / s.count, arm), (60, 6_000, false));
}

#[test]
fn an_epoch_with_too_few_entries_has_no_reward() {
    let mut r = replica(0, &[0, 1, 2], 32);
    phase(&mut r, Phase::Explore(0));
    r.tuning.trials.insert(128, 0);
    report(&mut r, 1, 128, &[stat(0, 31, 4)]);
    assert!(r.measured().is_empty());
    report(&mut r, 2, 128, &[stat(0, 1, 4)]);
    assert_eq!(r.measured().get(&NodeId(0)).map(|s| s.count), Some(32));
}

#[test]
fn an_entry_proposed_before_its_epoch_began_does_not_count() {
    let mut r = replica(0, &[0, 1, 2], 1);
    r.tuning.epoch_started = Timestamp(100);
    r.now = Timestamp(5_000_000);
    r.note_commit(Slot(1), Timestamp(99));
    assert!(r.tuning.current.is_empty());
    r.note_commit(Slot(1), Timestamp(100));
    assert_eq!(r.tuning.current.iter().map(|s| (s.leader, s.count)).collect::<Vec<_>>(), [(NodeId(0), 1)]);
}

#[test]
fn reports_from_outside_the_order_are_ignored() {
    let mut r = replica(0, &[0, 1, 2], 1);
    report(&mut r, 7, 128, &[stat(0, 10, 4)]);
    report(&mut r, 1, 128, &[stat(9, 10, 4)]);
    assert_eq!(r.epoch_stat(128, NodeId(0)).0.count, 0);
    assert!(r.tuning.reports.get(&128).is_none_or(|e| e.values().all(|(_, s)| s.is_empty())));
}

#[test]
fn an_idle_replica_with_tuning_on_has_no_deadline() {
    let r = replica(0, &[0, 1, 2], 1);
    assert!(r.cfg.tuning);
    assert_eq!(r.next_deadline(), None);
}

#[test]
fn an_exploration_tries_every_member_twice_then_sorts() {
    let mut r = replica(0, &[0, 1, 2], 1);
    // Trial t is led by the members in id order, two epochs each, then one uncounted epoch.
    let expected = [
        (alloc::vec![0, 1, 2], Phase::Explore(1)),
        (alloc::vec![1, 0, 2], Phase::Explore(2)),
        (alloc::vec![1, 0, 2], Phase::Explore(3)),
        (alloc::vec![2, 0, 1], Phase::Explore(4)),
        (alloc::vec![2, 0, 1], Phase::Explore(5)),
        (alloc::vec![0, 1, 2], Phase::Explore(6)),
    ];
    for (t, want) in expected.iter().enumerate() {
        phase(&mut r, Phase::Explore(t as u16));
        r.decide_tuning();
        assert_eq!(proposed(&r).as_ref(), Some(want), "after trial {t}");
        clear(&mut r);
    }
    // After the uncounted epoch, the measured members, best first.
    for (t, (member, ms)) in [(0u16, 5), (1, 5), (2, 3), (3, 3), (4, 9), (5, 9)]
        .into_iter()
        .map(|(t, ms)| (t, (u32::from(t) / 2, ms)))
    {
        let first = 1_000 + u64::from(t) * u64::from(E);
        r.tuning.trials.insert(first, t);
        report(&mut r, 1, first, &[stat(member, 10, ms)]);
    }
    phase(&mut r, Phase::Explore(6));
    r.decide_tuning();
    assert_eq!(proposed(&r), Some((alloc::vec![1, 0, 2], Phase::Exploit)));
}

#[test]
fn a_member_never_measured_is_ranked_last() {
    let mut r = replica(0, &[0, 1, 2], 4);
    for t in 0u16..6 {
        let first = 1_000 + u64::from(t) * u64::from(E);
        r.tuning.trials.insert(first, t);
        // Member 1 crashed during its trial: rotation left it without enough entries.
        let member = u32::from(t) / 2;
        let count = if member == 1 { 1 } else { 10 };
        report(&mut r, 2, first, &[stat(member, count, 1 + u64::from(member))]);
    }
    phase(&mut r, Phase::Explore(6));
    r.decide_tuning();
    assert_eq!(proposed(&r), Some((alloc::vec![0, 2, 1], Phase::Exploit)));
}

#[test]
fn a_leader_that_missed_part_of_the_exploration_measures_again() {
    let mut r = replica(0, &[0, 1, 2], 1);
    // It restarted during the last trials: only trials 4 and 5 are known here.
    for t in [4u16, 5] {
        r.tuning.trials.insert(1_000 + u64::from(t), t);
    }
    phase(&mut r, Phase::Explore(6));
    r.decide_tuning();
    assert_eq!(proposed(&r), Some((alloc::vec![0, 1, 2], Phase::Explore(0))));
}

/// Exploiting with member 1 next, stored at 10 ms ±`next_spread`, and the leader's latest
/// epoch at `latest_ms` ±2 ms.
fn exploiting(latest_ms: u64, trying_arm: bool) -> Replica {
    let mut r = replica(0, &[0, 1, 2], 8);
    phase(&mut r, Phase::Exploit);
    r.tuning.period_from = Some(Timestamp(0));
    r.tuning.rewards.insert(NodeId(1), spread(1, 64, 10, 2));
    r.tuning.epoch_first = 1_000;
    r.on_epoch_report(NodeId(2), Slot(800), None, trying_arm, alloc::vec![spread(0, 64, latest_ms, 2)]);
    r
}

#[test]
fn the_leader_explores_again_only_when_significantly_behind_the_next() {
    // 2 ms spread over 64 entries each: a standard error of 0.35 ms on the difference.
    let mut r = exploiting(12, false);
    r.decide_tuning();
    assert_eq!(proposed(&r), Some((alloc::vec![0, 1, 2], Phase::Explore(0))));
    for near in [10, 11] {
        let mut r = exploiting(near, false);
        r.decide_tuning();
        assert_eq!(proposed(&r), None, "{near} ms is within noise of 10 ms");
    }
}

#[test]
fn an_epoch_with_a_hedge_trial_does_not_trigger_exploration() {
    let mut r = exploiting(20, true);
    r.decide_tuning();
    assert_eq!(proposed(&r), None);
}

#[test]
fn the_period_ends_in_a_new_exploration() {
    let mut r = exploiting(10, false);
    r.now = Timestamp(r.cfg.tuning_reexplore);
    r.decide_tuning();
    assert_eq!(proposed(&r), Some((alloc::vec![0, 1, 2], Phase::Explore(0))));
    // Off (0): only falling behind explores.
    let mut r = exploiting(10, false);
    r.cfg.tuning_reexplore = 0;
    r.now = Timestamp(u64::MAX / 2);
    r.decide_tuning();
    assert_eq!(proposed(&r), None);
}

#[test]
fn a_fresh_cluster_explores_at_its_first_decision() {
    let mut r = replica(0, &[0, 1, 2], 1);
    assert_eq!(r.tuning.period_from, None);
    r.decide_tuning();
    assert_eq!(proposed(&r), Some((alloc::vec![0, 1, 2], Phase::Explore(0))));
}

#[test]
fn an_operator_change_ends_the_exploration_and_clears_what_was_measured() {
    let mut r = exploiting(10, false);
    r.now = Timestamp(42);
    r.enter_phase(Phase::Monitor, 2_000);
    assert!(r.tuning.rewards.is_empty() && r.tuning.trials.is_empty());
    assert_eq!(r.tuning.period_from, Some(Timestamp(42)));
    // Nothing to compare until the period ends.
    r.on_epoch_report(NodeId(2), Slot(1_900), None, false, alloc::vec![stat(0, 64, 50)]);
    phase(&mut r, Phase::Monitor);
    r.tuning.epoch_first = 2_000;
    r.decide_tuning();
    assert_eq!(proposed(&r), None);
}

#[test]
fn the_next_member_that_reports_proposes_for_a_leader_without_tuning() {
    // Member 1 is next after leader 0, which runs without tuning and sends no reports.
    let mut r = replica(1, &[0, 1, 2], 1);
    phase(&mut r, Phase::Explore(0));
    r.tuning.live = true;
    r.tuning.epoch_first = 0;
    assert_eq!(r.tuning_deputy(), Some(NodeId(1)));
    // The epoch ran past its end with no change on its way.
    r.applied_tuning.anchor = 0;
    r.tuning_applied(Slot(u64::from(E) - 1));
    assert_eq!(proposed(&r), Some((alloc::vec![0, 1, 2], Phase::Explore(1))));
}

#[test]
fn the_leader_decides_lag_slots_before_the_epoch_ends() {
    let mut r = replica(0, &[0, 1, 2], 1);
    r.tuning.epoch_first = 0;
    r.tuning_applied(Slot(u64::from(E) - LAG - 2));
    assert_eq!(proposed(&r), None, "not yet");
    r.tuning_applied(Slot(u64::from(E) - LAG - 1));
    assert_eq!(proposed(&r), Some((alloc::vec![0, 1, 2], Phase::Explore(0))));
}

#[test]
fn a_change_taking_effect_starts_an_epoch_and_reports_the_last_one() {
    let mut r = replica(0, &[0, 1, 2], 1);
    r.tuning.epoch_first = 0;
    r.tuning.current = alloc::vec![stat(0, 3, 4)];
    r.applied_tuning.pending.push((90, Tag { phase: Phase::Explore(0), epoch_slots: E }));
    r.tuning_applied(Slot(89));
    assert_eq!((r.tuning.epoch_first, r.applied_tuning.anchor), (90, 90));
    assert_eq!(r.applied_tuning.tag.phase, Phase::Explore(0));
    assert_eq!(r.tuning.trials.get(&90), Some(&0));
    assert_eq!(r.epoch_stat(0, NodeId(0)).0.count, 3, "its own report is kept");
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    r.flush();
    r.poll_actions(&mut out);
}

#[test]
fn the_tuning_state_survives_a_snapshot() {
    let mut r = replica(0, &[0, 1, 2], 1);
    let state = TuningState {
        tag: Tag { phase: Phase::Explore(3), epoch_slots: 200 },
        anchor: 600,
        pending: alloc::vec![(700, Tag { phase: Phase::Explore(4), epoch_slots: 200 })],
    };
    r.applied_tuning = state.clone();
    let decoded = CoreState::decode(&r.core_state(), &r.cfg.members).unwrap();
    assert_eq!(decoded.tuning, state);
}

fn arms(means_ms: [u64; 4], spread_ms: u64) -> Vec<LatencyStat> {
    means_ms.iter().map(|&ms| spread(0, 64, ms, spread_ms)).collect()
}

#[test]
fn the_default_multiplier_stays_unless_another_is_significantly_faster() {
    use super::tuning::best_arm;
    // Within noise of 4's 5 ms: kept.
    assert_eq!(best_arm(&arms([6, 5, 5, 7], 4)), 4);
    // 2 is 3 ms faster with a 0.35 ms standard error: taken.
    assert_eq!(best_arm(&arms([9, 2, 5, 7], 2)), 2);
    // Nothing measured under 4: kept.
    let mut none = arms([1, 1, 5, 1], 1);
    none[2] = LatencyStat::new(NodeId(0));
    assert_eq!(best_arm(&none), 4);
}

/// Ends `n` epochs of own entries at `ms` each (64 of them).
fn end_epochs(r: &mut Replica, n: usize, ms: u64) {
    for _ in 0..n {
        r.hedge_epoch_ended(&[spread(0, 64, ms, 1)]);
    }
}

#[test]
fn each_arm_runs_two_counted_epochs_while_exploiting_then_the_best_is_kept() {
    let mut r = replica(0, &[0, 1, 2], 8);
    phase(&mut r, Phase::Exploit);
    end_epochs(&mut r, 1, 5);
    assert_eq!((r.tuning.hedge.trying, r.hedge_multiplier()), (Some((0, 0)), 1));
    // An epoch with too few entries does not count.
    r.hedge_epoch_ended(&[stat(0, 2, 5)]);
    assert_eq!(r.tuning.hedge.trying, Some((0, 0)));
    for (arm, ms) in [(1, 9), (2, 3), (4, 5)] {
        assert_eq!(r.hedge_multiplier(), arm);
        end_epochs(&mut r, 2, ms);
    }
    assert_eq!(r.hedge_multiplier(), 8);
    end_epochs(&mut r, 2, 7);
    assert_eq!((r.tuning.hedge.trying, r.hedge_multiplier()), (None, 2));
}

#[test]
fn the_multiplier_does_not_change_while_the_leader_is_explored() {
    let mut r = replica(0, &[0, 1, 2], 8);
    phase(&mut r, Phase::Explore(3));
    end_epochs(&mut r, 5, 5);
    assert_eq!((r.tuning.hedge.trying, r.hedge_multiplier()), (None, 4));
    // An exploration starting during the trials restarts them at the next exploit.
    phase(&mut r, Phase::Exploit);
    end_epochs(&mut r, 3, 5);
    assert!(r.tuning.hedge.trying.is_some());
    phase(&mut r, Phase::Explore(0));
    end_epochs(&mut r, 1, 5);
    assert_eq!((r.tuning.hedge.trying, r.hedge_multiplier(), r.tuning.hedge.due), (None, 4, true));
}

#[test]
fn only_the_hedging_schedule_uses_the_tuned_multiplier() {
    let mut r = replica(0, &[0, 1, 2], 8);
    r.commit_latency = Some(10 * 1_000_000);
    r.tuning.hedge.multiplier = 1;
    assert_eq!(r.tuned_hedge_delay(), 10_000_000);
    // Silence detection and stepping aside keep four times the commit latency.
    assert_eq!(r.hedge_delay(), 40_000_000);
    r.cfg.hedge_tuning = false;
    assert_eq!(r.tuned_hedge_delay(), r.hedge_delay());
    // Every arm stays within the hedge bounds.
    r.cfg.hedge_tuning = true;
    for m in super::tuning::ARMS {
        r.tuning.hedge.multiplier = m;
        let d = r.tuned_hedge_delay();
        assert!((r.cfg.hedge_min..=r.cfg.hedge_max).contains(&d), "{m}: {d}");
    }
}

#[test]
fn the_default_index_names_the_default_arm() {
    assert_eq!(super::tuning::ARMS[2], 4);
}

#[test]
fn a_replica_that_skipped_slots_ranks_from_the_trials_its_reports_name() {
    // It installed a snapshot during the exploration: it never applied trials 0 to 5, but
    // the reports say which epoch ran which.
    let mut r = replica(0, &[0, 1, 2], 1);
    for (t, ms) in [(0u16, 5), (1, 5), (2, 3), (3, 3), (4, 9), (5, 9)] {
        let first = 1_000 + u64::from(t) * u64::from(E);
        r.on_epoch_report(
            NodeId(1),
            Slot(first),
            Some(t),
            false,
            alloc::vec![stat(u32::from(t) / 2, 10, ms)],
        );
    }
    r.applied_tuning = TuningState {
        tag: Tag { phase: Phase::Explore(6), epoch_slots: E },
        anchor: 1_768,
        pending: Vec::new(),
    };
    r.emitted = 1_800;
    r.tuning.rewards.insert(NodeId(2), stat(2, 1, 1));
    r.resync_tuning();
    assert_eq!((r.tuning.epoch_first, r.tuning.trials.get(&1_768)), (1_768, Some(&6)));
    r.decide_tuning();
    assert_eq!(proposed(&r), Some((alloc::vec![1, 0, 2], Phase::Exploit)));
    // The exploration ended in the skipped slots: the table becomes the stored rewards.
    clear(&mut r);
    phase(&mut r, Phase::Exploit);
    r.resync_tuning();
    assert!(r.tuning.trials.is_empty());
    assert_eq!(r.led_latency(NodeId(1)), Some(3_000_000));
}

#[test]
fn only_the_latest_exploration_s_trials_are_kept() {
    let mut r = replica(0, &[0, 1, 2], 1);
    for (first, t) in [(100, 4u16), (200, 5), (300, 0), (400, 1)] {
        r.on_epoch_report(NodeId(1), Slot(first), Some(t), false, Vec::new());
    }
    assert_eq!(r.tuning.trials.keys().copied().collect::<Vec<_>>(), [300, 400]);
    // A trial this membership cannot have is not recorded.
    r.on_epoch_report(NodeId(1), Slot(500), Some(7), false, Vec::new());
    assert!(!r.tuning.trials.contains_key(&500));
}

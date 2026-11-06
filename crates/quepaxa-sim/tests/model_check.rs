//! Delay-bounded model checking of the real replica.

use quepaxa_sim::model::{ModelConfig, check};

fn budget(default: usize) -> usize {
    std::env::var("QPX_MODEL_STATES").ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn depth(default: usize) -> usize {
    std::env::var("QPX_MODEL_DEPTH").ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

/// Per-scenario deviation budget; nightly adds `QPX_MODEL_EXTRA_DEVIATIONS` to each.
fn deviations(default: u32) -> u32 {
    let extra: u32 =
        std::env::var("QPX_MODEL_EXTRA_DEVIATIONS").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    default + extra
}

#[test]
fn leader_and_follower_writes_with_a_crash_are_safe() {
    let cfg = ModelConfig {
        nodes: 3,
        proposals: vec![0, 1],
        deviations: deviations(1),
        crashes: 1,
        max_depth: depth(200),
        max_states: budget(1_000_000),
        ..ModelConfig::new(Vec::new(), 0, 0)
    };
    let report = check(&cfg).unwrap_or_else(|e| panic!("{e}"));
    println!("{report:?}");
    assert!(report.complete > 0, "{report:?}");
    // Every schedule within the bounds is explored (states at the depth bound are shown to
    // settle under the default schedule), whatever bounds the run uses.
    assert!(report.exhausted, "{report:?}");
}

/// Targets the durability rules: one leader write, a crash that may lose any suffix of the
/// write in progress, and two deviations (enough to partition the leader after it decides).
#[test]
fn leader_write_survives_a_crash_and_a_partition() {
    let cfg = ModelConfig {
        nodes: 3,
        proposals: vec![0],
        deviations: deviations(2),
        crashes: 1,
        max_depth: depth(200),
        max_states: budget(10_000_000),
        ..ModelConfig::new(Vec::new(), 0, 0)
    };
    let report = check(&cfg).unwrap_or_else(|e| panic!("{e}"));
    println!("{report:?}");
    assert!(report.complete > 0, "{report:?}");
    assert!(report.exhausted, "{report:?}");
}

#[test]
fn contending_followers_are_safe() {
    let cfg = ModelConfig {
        nodes: 3,
        proposals: vec![1, 2],
        deviations: deviations(2),
        crashes: 0,
        max_depth: depth(200),
        max_states: budget(1_000_000),
        ..ModelConfig::new(Vec::new(), 0, 0)
    };
    let report = check(&cfg).unwrap_or_else(|e| panic!("{e}"));
    println!("{report:?}");
    assert!(report.complete > 0, "{report:?}");
    // Every schedule within the bounds is explored (states at the depth bound are shown to
    // settle under the default schedule), whatever bounds the run uses.
    assert!(report.exhausted, "{report:?}");
}

/// A read at another replica, requested once a write is acknowledged, must cover it, under
/// a crash (that may lose an unsynced register) and a partition of any replica.
#[test]
fn a_read_after_an_acknowledged_write_covers_it() {
    let cfg = ModelConfig {
        nodes: 3,
        proposals: vec![0],
        reads: vec![2],
        deviations: deviations(1),
        crashes: 1,
        max_depth: depth(200),
        max_states: budget(2_000_000),
        ..ModelConfig::new(Vec::new(), 0, 0)
    };
    let report = check(&cfg).unwrap_or_else(|e| panic!("{e}"));
    println!("{report:?}");
    assert!(report.complete > 0, "{report:?}");
    assert!(report.exhausted, "{report:?}");
}

const MS: u64 = 1_000_000;

/// The leader goes silent once it has committed a write from each follower: they suspect
/// it, and the next member (the deputy) proposes the other follower's write.
#[test]
fn a_silent_leader_hands_follower_writes_to_a_deputy() {
    let cfg = ModelConfig {
        later: vec![2],
        silenced: Some(0),
        link_delay: 2 * MS,
        max_depth: depth(200),
        max_states: budget(2_000_000),
        ..ModelConfig::new(vec![1, 2], deviations(1), 0)
    };
    let report = check(&cfg).unwrap_or_else(|e| panic!("{e}"));
    println!("{report:?}");
    assert!(report.served > 0 && report.exhausted, "{report:?}");
    assert!(report.deputy > 0 && report.hedged > 0, "{report:?}");
}

/// A read at the deputy, requested once a first write is acknowledged, covers it while the
/// leader goes silent and another write is handed on to the deputy.
#[test]
fn a_read_while_writes_go_to_the_deputy_covers_it() {
    let cfg = ModelConfig {
        later: vec![2],
        reads: vec![1],
        silenced: Some(0),
        link_delay: 2 * MS,
        max_depth: depth(200),
        max_states: budget(2_000_000),
        ..ModelConfig::new(vec![1, 2], deviations(1), 0)
    };
    let report = check(&cfg).unwrap_or_else(|e| panic!("{e}"));
    println!("{report:?}");
    assert!(report.served > 0 && report.exhausted, "{report:?}");
    assert!(report.deputy > 0, "{report:?}");
}

/// The leader's disk is slower than every timeout: the followers commit without it, and it
/// steps aside, handing its own write on.
#[test]
fn a_leader_stuck_on_its_disk_steps_aside() {
    let cfg = ModelConfig {
        slow_disk: Some(0),
        max_depth: depth(200),
        max_states: budget(2_000_000),
        ..ModelConfig::new(vec![0, 1], deviations(1), 0)
    };
    let report = check(&cfg).unwrap_or_else(|e| panic!("{e}"));
    println!("{report:?}");
    assert!(report.complete > 0 && report.exhausted, "{report:?}");
    assert!(report.stepped_aside > 0, "{report:?}");
}

/// The leader's messages are slower than every timeout: a follower hedges its write.
#[test]
fn a_slow_leader_is_hedged_around() {
    let cfg = ModelConfig {
        slow_net: Some(0),
        max_depth: depth(200),
        max_states: budget(2_000_000),
        ..ModelConfig::new(vec![1], deviations(1), 0)
    };
    let report = check(&cfg).unwrap_or_else(|e| panic!("{e}"));
    println!("{report:?}");
    assert!(report.complete > 0 && report.exhausted, "{report:?}");
    assert!(report.hedged > 0, "{report:?}");
}

/// A spare replaces the leader while a write commits, with a crash-restart anywhere (losing
/// any suffix of the write in progress): every replica applies the same change in the same
/// slot. Nightly's extra deviation adds a partition, a lost message or a reordering.
#[test]
fn a_member_replaced_during_a_write_with_a_crash_is_agreed() {
    replace_a_member(deviations(0), 1);
}

/// The same change with any one deviation: a partition of any replica, a lost or duplicated
/// message, a reordering or an early timer.
#[test]
fn a_member_replaced_during_a_write_with_a_partition_is_agreed() {
    replace_a_member(deviations(1), 0);
}

fn replace_a_member(deviations: u32, crashes: u32) {
    let cfg = ModelConfig {
        spares: 1,
        reconfigure: Some((1, vec![1, 2, 3])),
        max_depth: depth(200),
        max_states: budget(2_000_000),
        ..ModelConfig::new(vec![2], deviations, crashes)
    };
    let report = check(&cfg).unwrap_or_else(|e| panic!("{e}"));
    println!("{report:?}");
    assert!(report.served > 0 && report.exhausted, "{report:?}");
    assert!(report.reconfigured > 0, "{report:?}");
}

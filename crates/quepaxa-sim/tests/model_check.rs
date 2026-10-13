//! Delay-bounded model checking of the real replica.

use quepaxa_sim::model::{ModelConfig, check};

fn budget(default: usize) -> usize {
    std::env::var("QPX_MODEL_STATES").ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn depth(default: usize) -> usize {
    std::env::var("QPX_MODEL_DEPTH").ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn deviations(default: u32) -> u32 {
    std::env::var("QPX_MODEL_DEVIATIONS").ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

#[test]
fn leader_and_follower_writes_with_a_crash_are_safe() {
    let cfg = ModelConfig {
        nodes: 3,
        proposals: vec![0, 1],
        deviations: deviations(2),
        crashes: 1,
        max_depth: depth(200),
        max_states: budget(1_000_000),
    };
    let report = check(&cfg).unwrap_or_else(|e| panic!("{e}"));
    println!("{report:?}");
    assert!(report.complete > 0, "{report:?}");
    // Every schedule within the bounds is explored (states at the depth bound are shown to
    // settle under the default schedule), whatever bounds the run uses.
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
    };
    let report = check(&cfg).unwrap_or_else(|e| panic!("{e}"));
    println!("{report:?}");
    assert!(report.complete > 0, "{report:?}");
    // Every schedule within the bounds is explored (states at the depth bound are shown to
    // settle under the default schedule), whatever bounds the run uses.
    assert!(report.exhausted, "{report:?}");
}

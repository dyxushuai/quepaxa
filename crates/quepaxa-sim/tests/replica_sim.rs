//! Replica-level deterministic simulation runs.

use quepaxa_sim::{Sim, SimConfig};

fn calm(nodes: usize, seed: u64) -> SimConfig {
    SimConfig {
        drop: 0,
        dup: 0,
        partition: 0,
        crash: 0,
        ..SimConfig::hostile(nodes, seed)
    }
}

#[test]
fn fault_free_cluster_commits_every_proposal() {
    for n in [1, 3, 5] {
        let report = Sim::new(calm(n, 1)).run().unwrap();
        assert!(
            report.entries > 100 && report.entries == report.proposals,
            "{report:?}"
        );
    }
}

#[test]
fn hostile_runs_stay_safe_and_converge() {
    let seeds: u64 = std::env::var("QPX_SIM_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    for seed in 0..seeds {
        let n = [3, 5][(seed % 2) as usize];
        if let Err(e) = Sim::new(SimConfig::hostile(n, seed)).run() {
            panic!("{e}");
        }
    }
}

#[test]
fn crash_heavy_runs_never_lose_acknowledged_state() {
    let seeds: u64 = std::env::var("QPX_SIM_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    for seed in 0..seeds {
        if let Err(e) = Sim::new(SimConfig::durability_stress(3, seed)).run() {
            panic!("{e}");
        }
    }
}

#[test]
fn same_seed_replays_identically() {
    let a = Sim::new(SimConfig::hostile(3, 7)).run().unwrap();
    let b = Sim::new(SimConfig::hostile(3, 7)).run().unwrap();
    assert_eq!(a.trace_hash, b.trace_hash);
}

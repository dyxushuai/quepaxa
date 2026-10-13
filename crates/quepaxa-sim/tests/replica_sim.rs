//! Replica-level deterministic simulation runs.

use quepaxa_sim::{Sim, SimConfig};

fn calm(nodes: usize, seed: u64) -> SimConfig {
    SimConfig { drop: 0, dup: 0, partition: 0, crash: 0, ..SimConfig::hostile(nodes, seed) }
}

#[test]
fn fault_free_cluster_commits_every_proposal() {
    for n in [1, 3, 5] {
        let report = Sim::new(calm(n, 1)).run().unwrap();
        assert!(
            report.entries > 100 && report.entries == report.proposals && report.reads > 50,
            "{report:?}"
        );
    }
}

#[test]
fn hostile_runs_stay_safe_and_converge() {
    let seeds: u64 = std::env::var("QPX_SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    for seed in 0..seeds {
        let n = [3, 5][(seed % 2) as usize];
        if let Err(e) = Sim::new(SimConfig::hostile(n, seed)).run() {
            panic!("{e}");
        }
    }
}

#[test]
fn crash_heavy_runs_never_lose_acknowledged_state() {
    let seeds: u64 = std::env::var("QPX_SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    for seed in 0..seeds {
        if let Err(e) = Sim::new(SimConfig::durability_stress(3, seed)).run() {
            panic!("{e}");
        }
    }
}

#[test]
fn replica_absent_for_the_whole_run_catches_up_quickly() {
    for seed in 0..5 {
        let cfg = SimConfig { absent: Some(2), heal_budget_ms: 300, ..calm(3, seed) };
        let report = Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"));
        assert!(report.slots > 100, "{report:?}");
    }
}

#[test]
fn leader_crash_stalls_commits_only_briefly() {
    for seed in 0..10 {
        let n = [3, 5][(seed % 2) as usize];
        let report = Sim::new(SimConfig { kill: Some((0, 1_000)), ..calm(n, seed) })
            .run()
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(report.max_commit_gap_ms < 600, "seed {seed}: {report:?}");
        assert!(report.slots > 150, "seed {seed}: commits continue without the leader: {report:?}");
    }
}

#[test]
fn compaction_keeps_memory_bounded_on_long_runs() {
    let cfg = SimConfig { fault_phase_ms: 20_000, propose: 600, ..calm(3, 11) };
    let report = Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"));
    assert!(report.slots > 1_000, "{report:?}");
    assert!(report.max_retained_slots <= 32 + 8, "{report:?}");
}

#[test]
fn absent_replica_recovers_through_a_peer_snapshot() {
    for seed in 0..5 {
        let report =
            Sim::new(SimConfig { absent: Some(2), ..calm(3, seed) }).run().unwrap_or_else(|e| panic!("{e}"));
        assert!(report.snapshots_installed >= 1, "seed {seed}: {report:?}");
    }
}

#[test]
fn same_seed_replays_identically() {
    let a = Sim::new(SimConfig::hostile(3, 7)).run().unwrap();
    let b = Sim::new(SimConfig::hostile(3, 7)).run().unwrap();
    assert_eq!(a.trace_hash, b.trace_hash);
}

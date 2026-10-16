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
    // Enough seeds to hit peers compacting past a snapshot while it is being installed.
    for seed in 0..50 {
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
        assert!(report.slots_after_kill > 50, "seed {seed}: commits continue without the leader: {report:?}");
    }
}

#[test]
fn compaction_keeps_memory_bounded_on_long_runs() {
    let cfg = SimConfig { fault_phase_ms: 20_000, propose: 600, ..calm(3, 11) };
    let report = Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"));
    assert!(report.slots > 1_000, "{report:?}");
    assert!(report.max_retained_slots <= 32 + 8, "{report:?}");
    assert!(report.local_batches <= 4, "committed batches leak: {} held", report.local_batches);
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
fn quiet_cluster_serves_multi_page_catch_up() {
    for seed in 0..5 {
        // Pages of ~4 KiB force many CatchUp/Entries rounds; after heal nobody else talks.
        let cfg = SimConfig {
            absent: Some(2),
            compact_every: None,
            max_message_bytes: 2 * 4096 + 1024,
            heal_budget_ms: 500,
            ..calm(3, seed)
        };
        let report = Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"));
        assert!(report.slots > 100, "seed {seed}: {report:?}");
        // Each answered page asks for the next at once; only lost pages wait for the timer
        // (5-8 retries per seed today, 50+ if pages waited for it).
        let retries: u64 = report.metrics.iter().map(|m| m.catch_up_retries).sum();
        assert!(retries <= 20, "seed {seed}: {retries} catch-up retries");
    }
}

#[test]
fn same_seed_replays_identically() {
    let a = Sim::new(SimConfig::hostile(3, 7)).run().unwrap();
    let b = Sim::new(SimConfig::hostile(3, 7)).run().unwrap();
    assert_eq!(a.trace_hash, b.trace_hash);
}

#[test]
fn metrics_count_what_happened() {
    let report = Sim::new(calm(3, 1)).run().unwrap();
    let total = |f: fn(&quepaxa::Metrics) -> u64| report.metrics.iter().map(f).sum::<u64>();
    assert!(total(|m| m.slots_decided) >= report.slots, "{report:?}");
    assert!(total(|m| m.fast_path_decisions) > 0, "{report:?}");
    assert!(total(|m| m.forwarded) > 0, "{report:?}");
    let hostile: Vec<_> = (0..10).map(|seed| Sim::new(SimConfig::hostile(3, seed)).run().unwrap()).collect();
    let total = |f: fn(&quepaxa::Metrics) -> u64| hostile.iter().flat_map(|r| &r.metrics).map(f).sum::<u64>();
    assert!(total(|m| m.retransmits) > 0 && total(|m| m.hedged) > 0);
}

#[test]
fn membership_changes_under_faults_stay_safe_and_converge() {
    // About 50x slower per seed than the other tests (two spares, 64-slot windows):
    // nightly's 100,000 seeds become 2,000 here.
    let seeds: u64 = std::env::var("QPX_SIM_SEEDS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map_or(40, |s| (s / 50).max(40));
    let mut applied = 0;
    for seed in 0..seeds {
        let n = [3, 5][(seed % 2) as usize];
        let cfg = SimConfig { spares: 2, reconfigure: 2, ..SimConfig::hostile(n, seed) };
        let report = Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"));
        applied += report.reconfigurations;
    }
    assert!(applied >= seeds, "reconfigurations actually happened: {applied}");
}

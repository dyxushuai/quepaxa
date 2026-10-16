//! Replica-level deterministic simulation runs.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use quepaxa_sim::{Sim, SimConfig};

/// `QPX_SIM_SEEDS`, or `default`.
fn seed_count(default: u64) -> u64 {
    std::env::var("QPX_SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

/// Runs `run(seed)` for seeds `0..count` on every core and returns how many it ran. With
/// `QPX_SIM_SHARD=i/n`, runs only the seeds equal to `i` modulo `n` (the nightly splits
/// the seeds across machines). The first failure stops the other threads and is re-raised.
fn for_seeds(count: u64, run: impl Fn(u64) + Sync) -> u64 {
    let (shard, shards) = std::env::var("QPX_SIM_SHARD").ok().map_or((0, 1), |s| {
        let (i, n) = s.split_once('/').expect("QPX_SIM_SHARD is i/n");
        (i.parse().expect("shard index"), n.parse().expect("shard count"))
    });
    assert!(shard < shards, "QPX_SIM_SHARD index must be below the count");
    let (next, ran, failed) = (AtomicU64::new(shard), AtomicU64::new(0), AtomicBool::new(false));
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                while !failed.load(Relaxed) {
                    let seed = next.fetch_add(shards, Relaxed);
                    if seed >= count {
                        break;
                    }
                    if let Err(panic) = catch_unwind(AssertUnwindSafe(|| run(seed))) {
                        failed.store(true, Relaxed);
                        resume_unwind(panic);
                    }
                    ran.fetch_add(1, Relaxed);
                }
            });
        }
    });
    ran.into_inner()
}

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
    for_seeds(seed_count(40), |seed| {
        let n = [3, 5][(seed % 2) as usize];
        if let Err(e) = Sim::new(SimConfig::hostile(n, seed)).run() {
            panic!("{e}");
        }
    });
}

#[test]
fn crash_heavy_runs_never_lose_acknowledged_state() {
    for_seeds(seed_count(40), |seed| {
        if let Err(e) = Sim::new(SimConfig::durability_stress(3, seed)).run() {
            panic!("{e}");
        }
    });
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
    // About 50x slower per seed than the other tests (two spares, 64-slot windows), so it
    // runs 1/50 of QPX_SIM_SEEDS (nightly: 2,000) unless QPX_SIM_MEMBERSHIP_SEEDS says how
    // many exactly (the canaries do).
    let env = |name: &str| std::env::var(name).ok().and_then(|s| s.parse::<u64>().ok());
    let seeds = env("QPX_SIM_MEMBERSHIP_SEEDS")
        .unwrap_or_else(|| env("QPX_SIM_SEEDS").map_or(40, |s| (s / 50).max(40)));
    let applied = AtomicU64::new(0);
    let ran = for_seeds(seeds, |seed| {
        let n = [3, 5][(seed % 2) as usize];
        let cfg = SimConfig { spares: 2, reconfigure: 2, ..SimConfig::hostile(n, seed) };
        let report = Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"));
        applied.fetch_add(report.reconfigurations, Relaxed);
    });
    let applied = applied.into_inner();
    assert!(applied >= ran, "reconfigurations actually happened: {applied}");
}

#[test]
fn removed_majority_waits_for_the_new_members() {
    // Seed 1534 removes two of three members; when they shut down as soon as they had
    // applied the old epoch themselves, the new members could never learn its last slots.
    let cfg = SimConfig { spares: 2, reconfigure: 2, ..SimConfig::hostile(3, 1534) };
    let report = Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"));
    assert!(report.reconfigurations > 0, "{report:?}");
}

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
fn writes_commit_within_the_bound_while_a_majority_is_connected() {
    // Without crashes and with rarer partitions, a majority stays connected for seconds, long
    // enough to hold its writes to the progress bound (at hostile rates a crash or a cut off
    // leader rescues a stuck write first); minorities are still cut off, links lose and
    // duplicate.
    for_seeds(seed_count(40), |seed| {
        let n = [3, 5][(seed % 2) as usize];
        let cfg = SimConfig { crash: 0, partition: 1, fault_phase_ms: 6_000, ..SimConfig::hostile(n, seed) };
        let report = Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"));
        assert!(report.held_writes > 0, "seed {seed}: no write was held to the bound");
    });
}

#[test]
fn a_write_cut_off_in_a_minority_is_not_held_to_the_bound() {
    let cfg = SimConfig { isolate: Some((2, 500, 5_000)), fault_phase_ms: 6_000, ..calm(3, 1) };
    let bound = cfg.progress_bound_ms().expect("calm links are connected");
    let report = Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"));
    let slowest = report
        .commits
        .iter()
        .filter(|c| c.0 == 2 && (500_000..5_500_000).contains(&c.1))
        .map(|c| (c.2 - c.1) / 1_000)
        .max()
        .expect("n2 took writes while cut off");
    assert!(slowest > bound, "n2's slowest write took {slowest} ms, within the {bound} ms bound");
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
    // runs 1/50 of QPX_SIM_SEEDS (nightly: 2,000; per PR: 120) unless
    // QPX_SIM_MEMBERSHIP_SEEDS says how many exactly (the canaries do).
    let env = |name: &str| std::env::var(name).ok().and_then(|s| s.parse::<u64>().ok());
    let seeds = env("QPX_SIM_MEMBERSHIP_SEEDS")
        .unwrap_or_else(|| env("QPX_SIM_SEEDS").map_or(120, |s| (s / 50).max(120)));
    let (applied, shrunk, grown) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));
    let ran = for_seeds(seeds, |seed| {
        let n = [3, 5][(seed % 2) as usize];
        let cfg = SimConfig { spares: 2, reconfigure: 2, ..SimConfig::hostile(n, seed) };
        let report = Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"));
        applied.fetch_add(report.reconfigurations, Relaxed);
        // Changes draw any size: down to one or two members, up to every replica.
        let sizes = || report.epochs.iter().map(Vec::len);
        shrunk.fetch_add(u64::from(sizes().min() <= Some(2)), Relaxed);
        grown.fetch_add(u64::from(sizes().max() > Some(n)), Relaxed);
    });
    let (applied, shrunk, grown) = (applied.into_inner(), shrunk.into_inner(), grown.into_inner());
    println!("{ran} runs: {applied} changes, {shrunk} shrank to one or two members, {grown} grew");
    assert!(applied >= ran, "reconfigurations actually happened: {applied}");
    assert!(shrunk > 0 && grown > 0, "{shrunk} runs shrank to one or two members, {grown} grew");
}

#[test]
fn placement_under_faults_and_reconfigurations_stays_safe_and_converges() {
    // Placement with periods short enough to move the leader within a run, under faults.
    // Half the seeds also change membership about twice a second and crash replicas; every
    // change and restart starts a hold, so there placement mostly waits, and moves happen in
    // the other half.
    let env = |name: &str| std::env::var(name).ok().and_then(|s| s.parse::<u64>().ok());
    // Half as many seeds as the membership test: each runs a longer fault phase.
    let seeds = env("QPX_SIM_MEMBERSHIP_SEEDS")
        .unwrap_or_else(|| env("QPX_SIM_SEEDS").map_or(60, |s| (s / 100).max(60)));
    let moves = AtomicU64::new(0);
    for_seeds(seeds, |seed| {
        // Mixed, so that every nightly shard (seeds equal modulo the shard count) runs both.
        let mix = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 62;
        let n = [3, 5][(mix % 2) as usize];
        let changes = mix / 2 == 0;
        let hostile = SimConfig::hostile(n, seed);
        let cfg = SimConfig {
            spares: 2,
            reconfigure: if changes { 2 } else { 0 },
            // Without changes, crashes (each restart starts a hold) give way to loss and
            // partitions alone, so moves happen.
            crash: if changes { hostile.crash } else { 0 },
            placement: Some((300, 600)),
            write_weights: Some(vec![(0, vec![1, 20, 1, 1, 1])]),
            fault_phase_ms: 5_000,
            ..hostile
        };
        let report = Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"));
        moves.fetch_add(report.metrics.iter().map(|m| m.placement_moves).sum(), Relaxed);
    });
    println!("placement moves: {}", moves.load(Relaxed));
    assert!(moves.into_inner() > 0, "placement moved the leader in some run");
}

#[test]
fn removed_majority_waits_for_the_new_members() {
    const SEED: u64 = 1576;
    // The seed replaces two of the three members with spares; when removed members shut
    // down as soon as they had applied the old epoch themselves, the new members could never
    // learn its last slots. The seed only picks the schedule: re-pick one that still removes
    // two of three for a spare when timing changes move it.
    let cfg = SimConfig { spares: 2, reconfigure: 2, ..SimConfig::hostile(3, SEED) };
    let report = Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"));
    let removes_a_majority = report.epochs.windows(2).any(|w| {
        let kept = w[1].iter().filter(|m| w[0].contains(m)).count();
        2 * kept < w[0].len() && kept < w[1].len()
    });
    assert!(removes_a_majority, "seed {SEED} no longer removes a majority: {:?}", report.epochs);
}

fn lan(seed: u64) -> SimConfig {
    SimConfig {
        drop: 0,
        dup: 0,
        partition: 0,
        crash: 0,
        read: 0,
        max_latency_ms: 1,
        links_ms: Some(vec![vec![0, 1, 1], vec![1, 0, 1], vec![1, 1, 0]]),
        fault_phase_ms: 1_500,
        propose: 1_000,
        ..SimConfig::hostile(3, seed)
    }
}

fn median(mut v: Vec<u64>) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

#[test]
fn a_leader_on_a_slow_machine_commits_slower() {
    let fast = Sim::new(lan(1)).run().unwrap();
    let slow = Sim::new(SimConfig { slow_cpu: Some((vec![0], 600)), ..lan(1) }).run().unwrap();
    let (fast, slow) = (median(fast.commit_latencies_us), median(slow.commit_latencies_us));
    println!("median commit latency: {fast} us fast, {slow} us with a slow leader");
    assert!(slow * 5 > fast * 6, "{slow} us is not 20% slower than {fast} us");
}

#[test]
fn no_slow_machine_leaves_the_schedule_unchanged() {
    // Hashes recorded before slow machines existed: the default configuration must not
    // draw from the random stream or reorder events differently.
    for (n, seed, hash) in [(3, 7, 0xe3c6_f575_7bda_3544_u64), (5, 3, 0x2e06_6063_127e_031b)] {
        let r = Sim::new(SimConfig::hostile(n, seed)).run().unwrap();
        assert_eq!(r.trace_hash, hash, "{n} replicas, seed {seed}");
    }
}

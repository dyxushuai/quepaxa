//! A disk that stops or slows down for a while: the leader's alone, a follower's alone, or
//! one disk shared by all three replicas (a writeback burst). Writes resume as soon as a
//! majority can write again, and followers do not hedge around a leader that is only slow.
//!
//! `cargo test --release -p quepaxa-sim --test disk_faults -- --nocapture` prints the numbers;
//! `QPX_SIM_SEEDS` runs more seeds.

use quepaxa_sim::{Report, Sim, SimConfig};

const STALL_AT_MS: u64 = 1_500;
const STALL_MS: u64 = 300;

fn seeds() -> u64 {
    std::env::var("QPX_SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(1)
}

fn run(stalled: Vec<usize>, seed: u64) -> Report {
    sim(SimConfig { disk_stall: Some((stalled, STALL_AT_MS, STALL_MS)), ..config(seed) })
}

fn config(seed: u64) -> SimConfig {
    SimConfig {
        drop: 0,
        dup: 0,
        partition: 0,
        crash: 0,
        read: 0,
        max_latency_ms: 1,
        links_ms: Some(vec![vec![0, 1, 1], vec![1, 0, 1], vec![1, 1, 0]]),
        retransmit_ms: 50,
        hedge_max_ms: 1_000,
        max_persist_ms: 2,
        compact_every: None,
        fault_phase_ms: 3_000,
        heal_budget_ms: 60_000,
        // Per mille per 1 ms tick: about 1,000 writes/s over the cluster.
        propose: 1_000,
        ..SimConfig::hostile(3, seed)
    }
}

fn sim(cfg: SimConfig) -> Report {
    Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"))
}

/// The slowest write proposed at `nodes` from just before the stall to a second after it.
fn worst(r: &Report, nodes: &[usize]) -> u64 {
    let (from, to) = ((STALL_AT_MS - 100) * 1_000, (STALL_AT_MS + STALL_MS + 1_000) * 1_000);
    r.commits
        .iter()
        .filter(|c| nodes.contains(&c.0) && c.1 >= from && c.1 < to)
        .map(|c| (c.2 - c.1) / 1_000)
        .max()
        .expect("writes commit around the stall")
}

#[test]
fn a_stalled_follower_disk_is_not_waited_for() {
    for seed in 0..seeds() {
        let max = worst(&run(vec![1], seed), &[0, 1, 2]);
        println!("follower disk stalled {STALL_MS} ms, seed {seed}: slowest write {max} ms");
        assert!(max < 50, "seed {seed}: slowest write {max} ms");
    }
}

#[test]
fn a_stalled_leader_disk_delays_followers_writes_less_than_the_stall() {
    for seed in 0..seeds() {
        let max = worst(&run(vec![0], seed), &[1, 2]);
        println!("leader disk stalled {STALL_MS} ms, seed {seed}: slowest follower write {max} ms");
        assert!(max < STALL_MS, "seed {seed}: slowest follower write {max} ms");
    }
}

#[test]
fn writes_resume_right_after_a_shared_disk_stall() {
    for seed in 0..seeds() {
        let r = run(vec![0, 1, 2], seed);
        // Nothing can be made durable meanwhile: once writes begun before the stall are done,
        // nothing is acknowledged until it ends.
        let (from, to) = ((STALL_AT_MS + 20) * 1_000, (STALL_AT_MS + STALL_MS) * 1_000);
        let through = r.commits.iter().filter(|c| c.2 > from && c.2 < to).count();
        assert_eq!(through, 0, "seed {seed}: {through} writes acknowledged through a stopped disk");
        let max = worst(&r, &[0, 1, 2]);
        println!("shared disk stalled {STALL_MS} ms, seed {seed}: slowest write {max} ms");
        assert!(max < STALL_MS + 30, "seed {seed}: slowest write {max} ms: hedging stretched it");
    }
}

/// Every write on the shared disk suddenly takes up to 40 ms instead of 2 for half a second.
/// The leader's commits take several times the hedge delay learned before, but the log keeps
/// growing: followers wait for it. Hedging every late entry instead (as before) contended
/// with the leader's slots: about 400 hedges and writes up to 210-410 ms, where a disk that
/// slow all along gives at most about 110 ms.
#[test]
fn a_sudden_shared_disk_slowdown_sets_off_no_hedge_storm() {
    for seed in 0..seeds() {
        let r = sim(SimConfig {
            disk_slow: Some((vec![0, 1, 2], STALL_AT_MS, 500)),
            slow_persist_ms: 40,
            ..config(seed)
        });
        let max = worst(&r, &[0, 1, 2]);
        let hedged: u64 = r.metrics.iter().map(|m| m.hedged).sum();
        println!("shared disk slowed, seed {seed}: slowest write {max} ms, {hedged} hedged");
        assert!(hedged < 150, "seed {seed}: {hedged} writes hedged around a slow leader");
        assert!(max < 300, "seed {seed}: slowest write {max} ms");
    }
}

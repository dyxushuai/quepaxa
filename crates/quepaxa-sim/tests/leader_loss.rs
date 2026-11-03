//! Losing the leader's region on a WAN: how long the other regions stop committing, and
//! how long their backlog takes to drain. Five regions with the `wan_leaders` round trips;
//! the leader (replica 0) sits in the region farthest from a majority, or the nearest, and
//! is killed.
//!
//! `cargo test --release -p quepaxa-sim --test leader_loss -- --nocapture` prints the
//! numbers; `QPX_SIM_SEEDS` runs more seeds.
//!
//! Before followers suspected a silent leader (seed 0, far leader, killed at 10 s), they
//! hedged forwarded entries one per batch from 11.1 s, but nothing committed until the dead
//! leader's in-flight slots were taken over at 12.4 s, 2δ after they were seen (δ about
//! 1.3 s), and the backlog drained slowly: a 2.6-4.6 s stall, p99 about 8 s.

use quepaxa_sim::{Report, Sim, SimConfig};

/// Round trips in ms: us-east, us-west, eu-west, ap-ne, sa-east (`wan_leaders`).
const RTT_MS: [[u64; 5]; 5] = [
    [0, 70, 80, 180, 120],
    [70, 0, 140, 110, 170],
    [80, 140, 0, 230, 190],
    [180, 110, 230, 0, 300],
    [120, 170, 190, 300, 0],
];
const FAR: usize = 3;
const NEAR: usize = 0;
const KILL_MS: u64 = 6_000;
const RUN_MS: u64 = 12_000;

fn seeds() -> u64 {
    std::env::var("QPX_SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(1)
}

/// One-way link delays with region `site` as replica 0 (the first leader).
fn links(site: usize) -> Vec<Vec<u64>> {
    let mut order: Vec<usize> = (0..5).collect();
    order.swap(0, site);
    order.iter().map(|&a| order.iter().map(|&b| RTT_MS[a][b] / 2).collect()).collect()
}

fn run(site: usize, seed: u64, kill: bool) -> Report {
    let cfg = SimConfig {
        drop: 0,
        dup: 0,
        partition: 0,
        crash: 0,
        read: 0,
        max_latency_ms: 5,
        links_ms: Some(links(site)),
        retransmit_ms: 400,
        hedge_max_ms: 3_000,
        compact_every: None,
        in_flight_slots: 8,
        fault_phase_ms: RUN_MS,
        heal_budget_ms: 120_000,
        // Per mille per 1 ms tick: about 200 writes/s over the cluster.
        propose: 200,
        kill: kill.then_some((0, KILL_MS)),
        ..SimConfig::hostile(5, seed)
    };
    Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"))
}

/// What the replicas other than the killed one saw after the kill: the longest stretch
/// without an acknowledged write at any of them, and the p99 commit latency of writes
/// proposed from the kill to four seconds after it.
fn after_kill(r: &Report) -> (u64, u64) {
    let (kill, end) = (KILL_MS * 1_000, RUN_MS * 1_000);
    let mut gap = 0;
    for node in 1..5 {
        let mut done: Vec<u64> =
            r.commits.iter().filter(|c| c.0 == node && c.2 >= kill && c.2 <= end).map(|c| c.2).collect();
        done.sort_unstable();
        let mut prev = kill;
        for t in done {
            gap = gap.max(t - prev);
            prev = t;
        }
        gap = gap.max(end - prev);
    }
    let mut lat: Vec<u64> = r
        .commits
        .iter()
        .filter(|c| c.0 != 0 && c.1 >= kill && c.1 < kill + 4_000_000)
        .map(|c| c.2 - c.1)
        .collect();
    assert!(!lat.is_empty(), "writes after the kill commit");
    lat.sort_unstable();
    (gap / 1_000, lat[(lat.len() - 1) * 99 / 100] / 1_000)
}

/// Writes resume within about two commit latencies (a far leader's is 400-600 ms here)
/// and the backlog drains within a few more.
fn recovers(site: usize, name: &str) {
    for seed in 0..seeds() {
        let (gap, p99) = after_kill(&run(site, seed, true));
        println!("{name} leader killed, seed {seed}: gap {gap} ms, p99 {p99} ms");
        assert!(gap < 1_500, "{name} leader, seed {seed}: writes stalled {gap} ms");
        assert!(p99 < 2_500, "{name} leader, seed {seed}: p99 {p99} ms after the kill");
    }
}

#[test]
fn losing_a_far_leader_stalls_the_other_regions_briefly() {
    recovers(FAR, "far");
}

#[test]
fn losing_a_near_leader_stalls_the_other_regions_briefly() {
    recovers(NEAR, "near");
}

#[test]
fn a_healthy_far_leader_is_not_hedged_around() {
    for seed in 0..seeds() {
        let r = run(FAR, seed, false);
        let hedged: u64 = r.metrics.iter().map(|m| m.hedged).sum();
        let entries = r.commits.len() as u64;
        println!("healthy far leader, seed {seed}: {hedged} of {entries} writes hedged");
        assert!(hedged * 100 <= entries, "seed {seed}: {hedged} of {entries} writes hedged");
    }
}

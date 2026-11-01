//! Losing the leader's region on a WAN: how long the other regions stop committing, and
//! how long their backlog takes to drain. Five regions with the `wan_leaders` round trips;
//! the leader (replica 0) sits in the region farthest from a majority and is killed.
//!
//! `cargo test -p quepaxa-sim --test leader_loss -- --nocapture` prints the numbers.
//!
//! Before the follower-side changes (seed 0, far leader, killed at 10 s): followers hedge
//! forwarded entries from 11.1 s, one entry per batch, but nothing commits until the dead
//! leader's in-flight slots are taken over at 12.4 s, 2δ after they were seen (δ about
//! 1.3 s); the backlog then drains in single-entry slots (p99 about 8 s).

use quepaxa_sim::{Report, Sim, SimConfig};

/// Round trips in ms: us-east, us-west, eu-west, ap-ne, sa-east (`wan_leaders`).
const RTT_MS: [[u64; 5]; 5] = [
    [0, 70, 80, 180, 120],
    [70, 0, 140, 110, 170],
    [80, 140, 0, 230, 190],
    [180, 110, 230, 0, 300],
    [120, 170, 190, 300, 0],
];
const KILL_MS: u64 = 10_000;
const RUN_MS: u64 = 25_000;

/// One-way link delays with region `site` as replica 0 (the first leader).
fn links(site: usize) -> Vec<Vec<u64>> {
    let mut order: Vec<usize> = (0..5).collect();
    order.swap(0, site);
    order.iter().map(|&a| order.iter().map(|&b| RTT_MS[a][b] / 2).collect()).collect()
}

fn run(site: usize, seed: u64) -> Report {
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
        kill: Some((0, KILL_MS)),
        ..SimConfig::hostile(5, seed)
    };
    Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"))
}

/// What the replicas other than the killed one saw after the kill: the longest stretch
/// without an acknowledged write at any of them, and the p99 commit latency of writes
/// proposed from the kill to five seconds after it.
fn after_kill(r: &Report) -> (u64, u64, usize) {
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
    }
    let mut lat: Vec<u64> = r
        .commits
        .iter()
        .filter(|c| c.0 != 0 && c.1 >= kill && c.1 < kill + 5_000_000)
        .map(|c| c.2 - c.1)
        .collect();
    lat.sort_unstable();
    let p99 = lat.get((lat.len().saturating_sub(1)) * 99 / 100).copied().unwrap_or(0);
    (gap / 1_000, p99 / 1_000, lat.len())
}

#[test]
fn a_lost_far_leader_stalls_the_other_regions() {
    for seed in 0..3 {
        let r = run(3, seed);
        let (gap, p99, n) = after_kill(&r);
        let hedged: u64 = r.metrics.iter().map(|m| m.hedged).sum();
        let forwarded: u64 = r.metrics.iter().map(|m| m.forwarded).sum();
        println!(
            "far leader, seed {seed}: gap {gap} ms, p99 {p99} ms over {n} writes, hedged {hedged}, forwards {forwarded}"
        );
        assert!(n > 0, "writes after the kill commit");
    }
}

#[test]
fn a_lost_near_leader_stalls_them_less() {
    for seed in 0..3 {
        let r = run(0, seed);
        let (gap, p99, n) = after_kill(&r);
        println!("near leader, seed {seed}: gap {gap} ms, p99 {p99} ms over {n} writes");
        assert!(n > 0, "writes after the kill commit");
    }
}

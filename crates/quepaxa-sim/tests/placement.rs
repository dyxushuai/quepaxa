//! Leader placement across five regions (`wan_leaders` round trips): the leader starts in
//! ap-ne, the region farthest from a majority, while every region writes; placement moves it
//! to us-east and leaves it there. Short periods (5 s sustained, 20 s hold) stand in for the
//! default minutes.
//!
//! `cargo test --release -p quepaxa-sim --test placement -- --nocapture` prints the runs;
//! `QPX_SIM_SEEDS` runs more seeds.

use quepaxa_sim::{Report, Sim, SimConfig, WriteWeights};

/// Round trips in ms: us-east, us-west, eu-west, ap-ne, sa-east (`wan_leaders`).
const RTT_MS: [[u64; 5]; 5] = [
    [0, 70, 80, 180, 120],
    [70, 0, 140, 110, 170],
    [80, 140, 0, 230, 190],
    [180, 110, 230, 0, 300],
    [120, 170, 190, 300, 0],
];
/// ap-ne is replica 0 (the first leader); us-east, swapped with it, is replica 3.
const AP_NE: usize = 3;
const US_EAST_REPLICA: usize = 3;
const SUSTAIN_MS: u64 = 5_000;
const HOLD_MS: u64 = 20_000;

fn seeds() -> u64 {
    std::env::var("QPX_SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(1)
}

/// One-way link delays with ap-ne as replica 0.
fn links() -> Vec<Vec<u64>> {
    let mut order: Vec<usize> = (0..5).collect();
    order.swap(0, AP_NE);
    order.iter().map(|&a| order.iter().map(|&b| RTT_MS[a][b] / 2).collect()).collect()
}

fn run(seed: u64, run_ms: u64, write_weights: Option<WriteWeights>) -> Report {
    let cfg = SimConfig {
        drop: 0,
        dup: 0,
        partition: 0,
        crash: 0,
        read: 0,
        max_latency_ms: 5,
        links_ms: Some(links()),
        retransmit_ms: 400,
        hedge_max_ms: 3_000,
        in_flight_slots: 8,
        fault_phase_ms: run_ms,
        heal_budget_ms: 120_000,
        // Per mille per 1 ms tick: about 100 writes/s over the cluster.
        propose: 100,
        placement: Some((SUSTAIN_MS, HOLD_MS)),
        write_weights,
        ..SimConfig::hostile(5, seed)
    };
    Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"))
}

fn moves(r: &Report) -> u64 {
    r.metrics.iter().map(|m| m.placement_moves).sum()
}

#[test]
fn the_leader_moves_from_ap_ne_to_us_east_once() {
    for seed in 0..seeds() {
        // Two sustained periods plus probing and the move's LAG, then several holds.
        let r = run(seed, 60_000, None);
        println!("seed {seed}: order {:?}, moves {}", r.final_order, moves(&r));
        assert_eq!(r.final_order.first(), Some(&US_EAST_REPLICA), "seed {seed}: {:?}", r.final_order);
        assert_eq!(moves(&r), 1, "seed {seed}: the leader stays once placed");
    }
}

#[test]
fn the_leader_follows_writes_that_shift_for_good() {
    for seed in 0..seeds() {
        // Writes come from ap-ne (replica 0) and us-west (replica 1) until 40 s, then from
        // eu-west (replica 2) and us-east (replica 3).
        let weights = Some((vec![10, 10, 0, 0, 0], Some((40_000, vec![0, 0, 10, 10, 0]))));
        let r = run(seed, 100_000, weights);
        println!("seed {seed}: order {:?}, moves {}", r.final_order, moves(&r));
        assert!(matches!(r.final_order.first(), Some(&2) | Some(&3)), "seed {seed}: {:?}", r.final_order);
    }
}

#[test]
fn a_leader_with_a_slow_disk_is_moved_back_at_most_once() {
    for seed in 0..seeds() {
        // us-east (replica 3) is placed, then its disk turns slow for good: rotation demotes
        // it, and placement may restore it once before its doubled exclusion holds it off.
        let cfg = SimConfig {
            drop: 0,
            dup: 0,
            partition: 0,
            crash: 0,
            read: 0,
            max_latency_ms: 5,
            links_ms: Some(links()),
            retransmit_ms: 400,
            hedge_max_ms: 3_000,
            in_flight_slots: 8,
            fault_phase_ms: 120_000,
            heal_budget_ms: 120_000,
            propose: 100,
            placement: Some((SUSTAIN_MS, HOLD_MS)),
            disk_slow: Some((vec![US_EAST_REPLICA], 30_000, 90_000)),
            slow_persist_ms: 3_000,
            ..SimConfig::hostile(5, seed)
        };
        let r = Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"));
        let aside: u64 = r.metrics.iter().map(|m| m.stepped_aside).sum();
        println!("seed {seed}: order {:?}, moves {}, stepped aside {aside}", r.final_order, moves(&r));
        assert!(aside > 0, "seed {seed}: the slow disk made the leader step aside");
        assert!(moves(&r) <= 2, "seed {seed}: placed once, restored at most once: {}", moves(&r));
    }
}

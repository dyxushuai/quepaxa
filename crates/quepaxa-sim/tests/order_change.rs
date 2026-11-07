//! How long an order-only change takes to come into force: decided in slot `s`, it governs
//! slots from `s + LAG` (64) on. Writes fill those slots; with none, the leader fills them
//! with empty batches after a hedge delay. Measured from the first replica to report the
//! change decided to the agreed log reaching its first slot.
//!
//! `cargo test --release -p quepaxa-sim --test order_change -- --nocapture` prints the
//! numbers (`--ignored` adds every write rate); `QPX_SIM_SEEDS` runs more seeds.

use quepaxa_sim::{Report, Sim, SimConfig};

/// Round trips in ms: us-east, us-west, eu-west, ap-ne, sa-east (`wan_leaders`).
const RTT_MS: [[u64; 5]; 5] = [
    [0, 70, 80, 180, 120],
    [70, 0, 140, 110, 170],
    [80, 140, 0, 230, 190],
    [180, 110, 230, 0, 300],
    [120, 170, 190, 300, 0],
];
const CHANGE_MS: u64 = 2_000;
const WRITES_AFTER_MS: u64 = 5_000;

fn seeds() -> u64 {
    std::env::var("QPX_SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(1)
}

fn links() -> Vec<Vec<u64>> {
    RTT_MS.iter().map(|row| row.iter().map(|rtt| rtt / 2).collect()).collect()
}

/// The leader (replica 0, us-east) hands the lead to us-west at `CHANGE_MS` while clients
/// write `per_s` times a second spread over all members, until `writes_ms`.
fn run(wan: bool, per_s: u32, writes_ms: u64, seed: u64) -> Report {
    let base = SimConfig {
        drop: 0,
        dup: 0,
        partition: 0,
        crash: 0,
        read: 0,
        in_flight_slots: 8,
        fault_phase_ms: writes_ms,
        heal_budget_ms: 600_000,
        // Per mille per 1 ms tick: writes per second.
        propose: per_s,
        reorder: Some((0, CHANGE_MS, vec![1, 0, 2, 3, 4])),
        ..SimConfig::hostile(5, seed)
    };
    let cfg = if wan {
        SimConfig {
            max_latency_ms: 5,
            links_ms: Some(links()),
            retransmit_ms: 400,
            hedge_max_ms: 3_000,
            ..base
        }
    } else {
        // The library's default timers.
        SimConfig { max_latency_ms: 1, retransmit_ms: 50, hedge_max_ms: 1_000, ..base }
    };
    Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"))
}

/// Milliseconds from the proposal to the decision, and from the decision to the change's
/// first slot. Writes stop `WRITES_AFTER_MS` after the proposal; the change must come into
/// force before that for the measurement to count them.
fn takes_ms(wan: bool, per_s: u32, seed: u64) -> (u64, u64) {
    let writes_end = CHANGE_MS + WRITES_AFTER_MS;
    let r = run(wan, per_s, writes_end, seed);
    let decided = r.reorder_decided_us.expect("the change is decided");
    let in_force = r.reorder_in_force_us.expect("the change comes into force");
    if per_s > 0 {
        assert!(in_force < writes_end * 1_000, "writes stopped before the change came into force");
    }
    (decided / 1_000 - CHANGE_MS, (in_force - decided) / 1_000)
}

fn in_force_within(wan: bool, per_s: u32, bound_ms: u64) {
    let net = if wan { "WAN" } else { "LAN" };
    for seed in 0..seeds() {
        let (decide, ms) = takes_ms(wan, per_s, seed);
        println!("{net} {per_s} writes/s, seed {seed}: decided after {decide} ms, in force {ms} ms later");
        assert!(ms < bound_ms, "{net} {per_s} writes/s, seed {seed}: in force after {ms} ms");
    }
}

/// The leader fills the slots with empty batches, a window of `in_flight_slots` at a time
/// after a hedge delay: about 3.6 s on the WAN, 0.25 s on a LAN.
#[test]
fn an_idle_cluster_takes_an_order_change_into_force_through_stalled_slots() {
    in_force_within(true, 0, 8_000);
    in_force_within(false, 0, 1_000);
}

/// Writes fill the slots sooner than empty batches would: about 0.8 s.
#[test]
fn an_order_change_is_in_force_within_seconds_at_100_writes_per_second_across_five_regions() {
    in_force_within(true, 100, 2_000);
}

/// The table in `docs/benchmarks.md`.
#[test]
#[ignore = "slow at 1,000 writes/s; prints the benchmarks table"]
fn table() {
    for wan in [true, false] {
        for per_s in [0, 1, 10, 100, 1_000] {
            in_force_within(wan, per_s, u64::MAX);
        }
    }
}

//! Leader-policy evaluation: the scenarios every leader policy is compared on (plan
//! `docs/plans/2026-11-08-1002-feat-paper-leader-tuning-plan.md`, R14). Each test prints
//! markdown table rows, one per seed, for `docs/benchmarks.md`; none asserts a result,
//! since the point is to compare policies across commits.
//!
//! `cargo nextest run --release -p quepaxa-sim --test evaluation --run-ignored only
//! --no-capture`; `QPX_SIM_SEEDS` (default 2) sets the seeds per scenario.

use quepaxa_sim::{Report, Sim, SimConfig, WriteWeights};

const REGIONS: [&str; 5] = ["us-east", "us-west", "eu-west", "ap-ne", "sa-east"];
/// Round trips in ms between the regions (`wan_leaders`).
const RTT_MS: [[u64; 5]; 5] = [
    [0, 70, 80, 180, 120],
    [70, 0, 140, 110, 170],
    [80, 140, 0, 230, 190],
    [180, 110, 230, 0, 300],
    [120, 170, 190, 300, 0],
];
const US_EAST: usize = 0;
const US_WEST: usize = 1;
const EU_WEST: usize = 2;
const AP_NE: usize = 3;
const SA_EAST: usize = 4;

/// The leader policy under evaluation, on every replica. Short periods stand in for the
/// default minutes so a run converges in simulated seconds.
fn policy(cfg: SimConfig) -> SimConfig {
    SimConfig { placement: Some((5_000, 20_000)), ..cfg }
}

fn seeds() -> u64 {
    std::env::var("QPX_SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(2)
}

/// Replica i runs in region `order[i]`; replica 0 leads first.
fn order_from(first: usize) -> Vec<usize> {
    let mut order = vec![first];
    order.extend((0..5).filter(|&r| r != first));
    order
}

/// Five regions, `first` leading at the start; `weights` per region (equal when none).
fn wan(seed: u64, first: usize, run_ms: u64, weights: Option<Vec<(u64, [u32; 5])>>) -> SimConfig {
    let order = order_from(first);
    let links = order.iter().map(|&a| order.iter().map(|&b| RTT_MS[a][b] / 2).collect()).collect();
    let write_weights: Option<WriteWeights> = weights.map(|steps| {
        steps.into_iter().map(|(at, w)| (at, order.iter().map(|&region| w[region]).collect())).collect()
    });
    SimConfig {
        drop: 0,
        dup: 0,
        partition: 0,
        crash: 0,
        read: 0,
        max_latency_ms: 5,
        links_ms: Some(links),
        retransmit_ms: 400,
        hedge_max_ms: 3_000,
        in_flight_slots: 8,
        fault_phase_ms: run_ms,
        heal_budget_ms: 120_000,
        // Per mille per 1 ms tick: about 100 writes/s over the cluster.
        propose: 100,
        write_weights,
        track_leader: true,
        ..SimConfig::hostile(5, seed)
    }
}

/// Five replicas on a LAN (1 ms one way), about 300 writes/s: enough for a 600 us/message
/// leader to commit about a third slower, light enough for the per-event invariant checks.
fn lan(seed: u64, run_ms: u64) -> SimConfig {
    SimConfig {
        drop: 0,
        dup: 0,
        partition: 0,
        crash: 0,
        read: 0,
        max_latency_ms: 1,
        links_ms: Some(vec![vec![1; 5]; 5]),
        fault_phase_ms: run_ms,
        propose: 300,
        track_leader: true,
        ..SimConfig::hostile(5, seed)
    }
}

fn run(cfg: SimConfig) -> Report {
    Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"))
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// Commit latencies (ms) of writes proposed from `from_ms` on: p50 and p99.
fn latency(r: &Report, from_ms: u64) -> (f64, f64) {
    let mut v: Vec<u64> = r.commits.iter().filter(|c| c.1 >= from_ms * 1_000).map(|c| c.2 - c.1).collect();
    v.sort_unstable();
    (percentile(&v, 0.5) as f64 / 1_000.0, percentile(&v, 0.99) as f64 / 1_000.0)
}

/// Prints one row: the final leader, how often and when the lead last moved, latency from
/// `from_ms` on, and the same against a reference run when given.
fn row(
    scenario: &str,
    seed: u64,
    r: &Report,
    name: impl Fn(usize) -> String,
    from_ms: u64,
    reference: Option<&Report>,
) {
    let (p50, p99) = latency(r, from_ms);
    let changes = r.leader_timeline.len().saturating_sub(1);
    let (since, leader) = r.leader_timeline.last().copied().unwrap_or((0, 0));
    let vs = reference.map_or(String::from("-"), |reference| {
        let (_, ref_p99) = latency(reference, from_ms);
        format!("{:.2}", p99 / ref_p99)
    });
    println!(
        "| {scenario} | {seed} | {} | {changes} | {:.1} | {p50:.1} | {p99:.1} | {vs} | {} |",
        name(leader),
        since as f64 / 1_000.0,
        r.max_commit_gap_ms
    );
}

fn region_of(first: usize) -> impl Fn(usize) -> String {
    let order = order_from(first);
    move |replica| REGIONS[order[replica]].to_owned()
}

/// Header for the rows above (printed by every test so each output stands alone).
fn header() {
    println!(
        "| Scenario | Seed | Final leader | Leader changes | Last change (s) | p50 (ms) | p99 (ms) | p99 / reference | Longest gap (ms) |"
    );
    println!("|---|---|---|---|---|---|---|---|---|");
}

#[test]
#[ignore = "evaluation run, minutes long"]
fn convergence_from_ap_ne() {
    header();
    for seed in 0..seeds() {
        let r = run(policy(wan(seed, AP_NE, 90_000, None)));
        let pinned = run(wan(seed, US_EAST, 90_000, None));
        row("convergence", seed, &r, region_of(AP_NE), 60_000, Some(&pinned));
    }
}

#[test]
#[ignore = "evaluation run, minutes long"]
fn writes_shift_for_good() {
    header();
    let (mut near_west, mut near_east) = ([0; 5], [0; 5]);
    (near_west[AP_NE], near_west[US_WEST]) = (10, 10);
    (near_east[EU_WEST], near_east[US_EAST]) = (10, 10);
    for seed in 0..seeds() {
        let steps = vec![(0, near_west), (40_000, near_east)];
        let r = run(policy(wan(seed, AP_NE, 100_000, Some(steps))));
        row("write shift", seed, &r, region_of(AP_NE), 80_000, None);
    }
}

#[test]
#[ignore = "evaluation run, minutes long"]
fn a_short_burst_from_far_regions() {
    header();
    let mut far = [0; 5];
    far[AP_NE] = 10;
    far[SA_EAST] = 10;
    for seed in 0..seeds() {
        let steps = vec![(0, [1; 5]), (30_000, far), (33_000, [1; 5])];
        let r = run(policy(wan(seed, US_EAST, 90_000, Some(steps))));
        let pinned = run(wan(seed, US_EAST, 90_000, None));
        row("burst", seed, &r, region_of(US_EAST), 0, Some(&pinned));
    }
}

#[test]
#[ignore = "evaluation run, minutes long"]
fn the_leader_crashes() {
    header();
    for seed in 0..seeds() {
        let r = run(policy(SimConfig { kill: Some((0, 30_000)), ..wan(seed, US_EAST, 60_000, None) }));
        row("leader crash", seed, &r, region_of(US_EAST), 30_000, None);
    }
}

#[test]
#[ignore = "evaluation run, minutes long"]
fn the_leader_disk_turns_slow() {
    header();
    for seed in 0..seeds() {
        let slow = SimConfig {
            disk_slow: Some((vec![0], 30_000, 30_000)),
            slow_persist_ms: 300,
            ..wan(seed, US_EAST, 90_000, None)
        };
        let r = run(policy(slow));
        row("slow disk", seed, &r, region_of(US_EAST), 30_000, None);
    }
}

#[test]
#[ignore = "evaluation run, minutes long"]
fn one_percent_loss() {
    header();
    for seed in 0..seeds() {
        let r = run(policy(SimConfig { drop: 10, ..wan(seed, US_EAST, 90_000, None) }));
        let pinned = run(SimConfig { drop: 10, ..wan(seed, US_EAST, 90_000, None) });
        row("1% loss", seed, &r, region_of(US_EAST), 0, Some(&pinned));
    }
}

#[test]
#[ignore = "evaluation run, minutes long"]
fn steady_load() {
    header();
    for seed in 0..seeds() {
        let r = run(policy(wan(seed, US_EAST, 180_000, None)));
        let pinned = run(wan(seed, US_EAST, 180_000, None));
        row("steady", seed, &r, region_of(US_EAST), 0, Some(&pinned));
    }
}

#[test]
#[ignore = "evaluation run, minutes long"]
fn a_slow_machine_leads_on_a_lan() {
    header();
    for seed in 0..seeds() {
        // Replica 0 leads first and handles each message 600 us slower (Fig. 10).
        let r = run(policy(SimConfig { slow_cpu: Some((vec![0], 600)), ..lan(seed, 60_000) }));
        let fast = run(SimConfig { slow_cpu: Some((vec![4], 600)), ..lan(seed, 60_000) });
        row("slow machine", seed, &r, |i| format!("replica {i}"), 40_000, Some(&fast));
    }
}

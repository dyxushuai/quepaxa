//! How much does the leader's placement matter on a WAN? Runs a fault-free five-region
//! cluster once per choice of leader site and prints client commit latency.
//!
//! `cargo run --release -p quepaxa-sim --example wan_leaders [seeds]`
//!
//! Round-trip times between regions are rough public-cloud figures (us-east, us-west,
//! eu-west, ap-northeast, sa-east); the initial leader is replica 0, so each run maps a
//! different region to replica 0.
use quepaxa_sim::{Sim, SimConfig};

const REGIONS: [&str; 5] = ["us-east", "us-west", "eu-west", "ap-ne", "sa-east"];
const RTT_MS: [[u64; 5]; 5] = [
    [0, 70, 80, 180, 120],
    [70, 0, 140, 110, 170],
    [80, 140, 0, 230, 190],
    [180, 110, 230, 0, 300],
    [120, 170, 190, 300, 0],
];

fn percentile(sorted: &[u64], p: f64) -> u64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let seeds: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(3);
    println!("leader site   p50 ms   p90 ms   p99 ms   samples");
    for (leader, site) in REGIONS.iter().enumerate() {
        // Replica i sits in region order[i]; replica 0 (the first leader) in `leader`.
        let order: Vec<usize> = std::iter::once(leader).chain((0..5).filter(|&r| r != leader)).collect();
        let links: Vec<Vec<u64>> =
            order.iter().map(|&a| order.iter().map(|&b| RTT_MS[a][b] / 2).collect()).collect();
        let mut lat = Vec::new();
        for seed in 0..seeds {
            let cfg = SimConfig {
                drop: 0,
                dup: 0,
                partition: 0,
                crash: 0,
                max_latency_ms: 5,
                links_ms: Some(links.clone()),
                fault_phase_ms: 20_000,
                heal_budget_ms: 120_000,
                propose: std::env::var("WAN_PROPOSE").ok().and_then(|s| s.parse().ok()).unwrap_or(100),
                read: 0,
                in_flight_slots: 8,
                ..SimConfig::hostile(5, seed)
            };
            let report = Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"));
            if std::env::var_os("WAN_METRICS").is_some() {
                eprintln!("{site} seed {seed}: {:?}", report.metrics);
            }
            lat.extend(report.commit_latencies_us);
        }
        lat.sort_unstable();
        let ms = |p| percentile(&lat, p) as f64 / 1000.0;
        println!("{site:<12} {:>7.0} {:>8.0} {:>8.0} {:>9}", ms(0.5), ms(0.9), ms(0.99), lat.len());
    }
}

//! Compound gray-fault convergence tests exercising all adaptive control loops together:
//! adaptive hedge delay (`δ`), liveness watch / silent-leader takeover, miss-based schedule
//! rotation, leader placement/ranking, online `PipelineTuner` (`tune_in_flight_slots`), and
//! referenced-batch content fetching (`Data`/`Fetch`) under simultaneous slow disks, packet loss,
//! and bursty workloads.
//!
//! `cargo test --release -p quepaxa-sim --test compound_gray_faults -- --nocapture` prints
//! summary metrics; `QPX_SIM_SEEDS` runs more seeds.

use quepaxa_sim::{Report, Sim, SimConfig};

/// Five-region WAN RTT matrix in ms: us-east, us-west, eu-west, ap-ne, sa-east.
const RTT_MS: [[u64; 5]; 5] = [
    [0, 70, 80, 180, 120],
    [70, 0, 140, 110, 170],
    [80, 140, 0, 230, 190],
    [180, 110, 230, 0, 300],
    [120, 170, 190, 300, 0],
];

fn seeds() -> u64 {
    std::env::var("QPX_SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(1)
}

fn wan_links_with_observer() -> Vec<Vec<u64>> {
    // Put ap-ne at index 0 (initial leader) so placement has an incentive to move toward us-east.
    // Observer (index 5) sits near us-east (same RTT profile as index 3).
    let mut order: Vec<usize> = (0..5).collect();
    order.swap(0, 3);
    order.push(0);
    order
        .iter()
        .enumerate()
        .map(|(i, &a)| {
            order.iter().enumerate().map(|(j, &b)| if i == j { 0 } else { RTT_MS[a][b] / 2 }).collect()
        })
        .collect()
}

fn lan_links(total: usize, one_way_ms: u64) -> Vec<Vec<u64>> {
    (0..total).map(|i| (0..total).map(|j| if i == j { 0 } else { one_way_ms }).collect()).collect()
}

fn total_moves(r: &Report) -> u64 {
    r.metrics.iter().map(|m| m.placement_moves).sum()
}

fn assert_loops_engaged(seed: u64, cfg: &SimConfig, r: &Report) {
    let data_sent: u64 = r.metrics.iter().map(|m| m.data_sent).sum();
    let fetches: u64 = r.metrics.iter().map(|m| m.fetches).sum();
    let mitigated: u64 = r.metrics.iter().map(|m| m.hedged + m.stepped_aside).sum();
    let max_w: u64 = r.metrics.iter().take(cfg.nodes).map(|m| m.in_flight_slots).max().unwrap_or(0);
    assert!(data_sent > 0, "seed {seed}: referenced-batch Data path was never exercised");
    assert!(fetches > 0, "seed {seed}: Fetch path was never exercised under compound faults");
    assert!(mitigated > 0, "seed {seed}: neither hedging nor step-aside engaged under slow/stalled disk");
    assert!(
        max_w > 1,
        "seed {seed}: PipelineTuner stayed pinned at W=1 on all voters after heal: {:?}",
        r.metrics.iter().take(cfg.nodes).map(|m| m.in_flight_slots).collect::<Vec<_>>()
    );
}

fn assert_clean_convergence(seed: u64, cfg: &SimConfig, r: &Report) {
    assert!(r.slots > 0 && r.entries > 0, "seed {seed}: no progress: {r:?}");
    assert_eq!(
        r.entries, r.proposals,
        "seed {seed}: every acknowledged/proposed write must commit ({} vs {})",
        r.entries, r.proposals
    );
    assert!(
        r.local_batches <= cfg.in_flight_slots as usize + 4,
        "seed {seed}: uncommitted local batches leaked: {}",
        r.local_batches
    );
    for (idx, m) in r.metrics.iter().enumerate() {
        assert!(
            m.in_flight_slots >= 1 && m.in_flight_slots <= u64::from(cfg.in_flight_slots),
            "seed {seed} n{idx}: invalid tuned in_flight_slots={}",
            m.in_flight_slots
        );
        if let Some(every) = cfg.compact_every {
            assert!(
                m.retained_slots <= every + 16,
                "seed {seed} n{idx}: retained_slots={} exceeds compact bound {}",
                m.retained_slots,
                every + 16
            );
        }
    }
}

#[test]
fn lan_slow_disk_packet_loss_and_burst_load_converge_with_all_loops() {
    for seed in 0..seeds() {
        let cfg = SimConfig {
            nodes: 3,
            seed,
            drop: 40,
            dup: 15,
            partition: 0,
            crash: 0,
            read: 40,
            observers: 1,
            max_latency_ms: 4,
            links_ms: Some(lan_links(4, 2)),
            retransmit_ms: 40,
            hedge_max_ms: 600,
            max_persist_ms: 2,
            slow_persist_ms: 250,
            // Leader disk turns gray-slow during the first burst, recovers, then load bursts again.
            disk_slow: Some((vec![0], 1_000, 2_500)),
            in_flight_slots: 8,
            tune_in_flight_slots: true,
            compact_every: Some(48),
            // Large payloads force mixed inline + referenced `Data`/`Fetch` paths.
            payload_bytes: Some((256, 1024)),
            // Calm -> 10x burst during gray fault -> quiet -> second burst after recovery.
            propose_schedule: Some(vec![(0, 50), (1_000, 500), (2_500, 40), (4_000, 450)]),
            fault_phase_ms: 5_500,
            heal_budget_ms: 60_000,
            track_leader: true,
            ..SimConfig::hostile(3, seed)
        };
        let r = Sim::new(cfg.clone()).run().unwrap_or_else(|e| panic!("{e}"));
        assert_clean_convergence(seed, &cfg, &r);
        assert_loops_engaged(seed, &cfg, &r);
        assert!(r.reads > 10, "seed {seed}: reads must keep completing under compound gray fault");
    }
}

#[test]
fn wan_compound_gray_fault_converges_without_placement_or_tuner_oscillation() {
    for seed in 0..seeds() {
        let cfg = SimConfig {
            nodes: 5,
            seed,
            drop: 25,
            dup: 10,
            partition: 0,
            crash: 0,
            read: 20,
            observers: 1,
            max_latency_ms: 8,
            links_ms: Some(wan_links_with_observer()),
            retransmit_ms: 350,
            hedge_max_ms: 2_500,
            max_persist_ms: 3,
            slow_persist_ms: 1_500,
            // Degrade the preferred region (us-east, replica 3) while placement + tuner are active.
            disk_slow: Some((vec![3], 10_000, 12_000)),
            in_flight_slots: 8,
            tune_in_flight_slots: true,
            placement: Some((4_000, 15_000)),
            compact_every: Some(64),
            payload_bytes: Some((256, 1024)),
            // Burst waves alternating with moderate load across the 28 s fault phase.
            propose_schedule: Some(vec![(0, 30), (8_000, 180), (14_000, 30), (20_000, 180), (25_000, 30)]),
            fault_phase_ms: 28_000,
            heal_budget_ms: 120_000,
            track_leader: true,
            ..SimConfig::hostile(5, seed)
        };
        let r = Sim::new(cfg.clone()).run().unwrap_or_else(|e| panic!("{e}"));
        assert_clean_convergence(seed, &cfg, &r);
        assert_loops_engaged(seed, &cfg, &r);
        let moves = total_moves(&r);
        assert!(
            (1..=4).contains(&moves),
            "seed {seed}: placement should move out of ap-ne without oscillating (moves={moves})"
        );
    }
}

#[test]
fn disk_stall_loss_burst_and_compaction_converge_with_tuner_and_fetching() {
    for seed in 0..seeds() {
        let cfg = SimConfig {
            nodes: 3,
            seed,
            drop: 50,
            dup: 20,
            partition: 0,
            crash: 0,
            read: 40,
            max_latency_ms: 6,
            links_ms: Some(lan_links(3, 3)),
            retransmit_ms: 40,
            hedge_max_ms: 800,
            max_persist_ms: 3,
            slow_persist_ms: 150,
            // Leader disk stalls completely while a follower's disk is simultaneously slowed.
            disk_stall: Some((vec![0], 1_200, 500)),
            disk_slow: Some((vec![1], 1_000, 2_500)),
            in_flight_slots: 8,
            tune_in_flight_slots: true,
            compact_every: Some(32),
            payload_bytes: Some((512, 1536)),
            propose_schedule: Some(vec![(0, 60), (1_000, 450), (3_000, 60), (4_000, 450)]),
            fault_phase_ms: 5_500,
            heal_budget_ms: 60_000,
            ..SimConfig::hostile(3, seed)
        };
        let r = Sim::new(cfg.clone()).run().unwrap_or_else(|e| panic!("{e}"));
        assert_clean_convergence(seed, &cfg, &r);
        assert_loops_engaged(seed, &cfg, &r);
    }
}

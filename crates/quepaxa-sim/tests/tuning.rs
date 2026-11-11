//! Leader tuning (the paper's §5.3) across five regions (`wan_leaders` round trips) and on
//! a LAN with one slow machine: every member leads in turn, then the order is the members
//! by the latency writers saw under each.
//!
//! `cargo test --release -p quepaxa-sim --test tuning -- --nocapture` prints the runs;
//! `QPX_SIM_SEEDS` runs more seeds.

use quepaxa_sim::{Report, Sim, SimConfig, TuningKnobs, WriteWeights};

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
/// No periodic exploration within these runs unless a test asks for one.
const NO_PERIOD: TuningKnobs =
    TuningKnobs { epoch_slots: 128, min_samples: 32, reexplore_ms: 0, hedge: true };

fn seeds() -> u64 {
    std::env::var("QPX_SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(1)
}

/// One-way link delays with ap-ne as replica 0.
fn links() -> Vec<Vec<u64>> {
    let mut order: Vec<usize> = (0..5).collect();
    order.swap(0, AP_NE);
    order.iter().map(|&a| order.iter().map(|&b| RTT_MS[a][b] / 2).collect()).collect()
}

fn wan(seed: u64, run_ms: u64, write_weights: Option<WriteWeights>, tuning: TuningKnobs) -> SimConfig {
    SimConfig {
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
        tuning: Some(tuning),
        write_weights,
        track_leader: true,
        ..SimConfig::hostile(5, seed)
    }
}

fn run(cfg: SimConfig) -> Report {
    Sim::new(cfg).run().unwrap_or_else(|e| panic!("{e}"))
}

fn moves(r: &Report) -> u64 {
    r.metrics.iter().map(|m| m.tuning_moves).sum()
}

/// Leader changes after virtual time `from_ms`.
fn changes_after(r: &Report, from_ms: u64) -> usize {
    r.leader_timeline.iter().filter(|(at, _)| *at > from_ms).count()
}

#[test]
fn every_member_leads_then_us_east_leads_for_good() {
    for seed in 0..seeds() {
        let r = run(wan(seed, 120_000, None, NO_PERIOD));
        println!(
            "seed {seed}: order {:?}, moves {}, timeline {:?}",
            r.final_order,
            moves(&r),
            r.leader_timeline
        );
        let leaders: std::collections::BTreeSet<usize> = r.leader_timeline.iter().map(|(_, l)| *l).collect();
        assert_eq!(leaders.len(), 5, "seed {seed}: every member led during the exploration");
        assert_eq!(r.final_order.first(), Some(&US_EAST_REPLICA), "seed {seed}: {:?}", r.final_order);
        let settled = r.leader_timeline.last().map_or(0, |(at, _)| *at);
        assert!(settled < 100_000, "seed {seed}: the exploration ended at {settled} ms");
    }
}

#[test]
fn a_slow_machine_ends_last() {
    for seed in 0..seeds() {
        // Five replicas on a LAN; replica 0 leads first and handles each message 600 us
        // slower (the paper's Fig. 10).
        let cfg = SimConfig {
            drop: 0,
            dup: 0,
            partition: 0,
            crash: 0,
            read: 0,
            max_latency_ms: 1,
            links_ms: Some(vec![vec![1; 5]; 5]),
            fault_phase_ms: 40_000,
            propose: 300,
            slow_cpu: Some((vec![0], 600)),
            tuning: Some(TuningKnobs { epoch_slots: 256, ..NO_PERIOD }),
            track_leader: true,
            ..SimConfig::hostile(5, seed)
        };
        let r = run(cfg);
        println!("seed {seed}: order {:?}, timeline {:?}", r.final_order, r.leader_timeline);
        assert_eq!(r.final_order.last(), Some(&0), "seed {seed}: {:?}", r.final_order);
    }
}

#[test]
fn the_leader_follows_writes_that_shift_for_good() {
    for seed in 0..seeds() {
        // Writes come from ap-ne (replica 0) and us-west (replica 1), then from eu-west
        // (replica 2) and us-east (replica 3); an exploration every 60 s finds the change.
        let weights = Some(vec![(0, vec![10, 10, 0, 0, 0]), (80_000, vec![0, 0, 10, 10, 0])]);
        let r = run(wan(seed, 200_000, weights, TuningKnobs { reexplore_ms: 60_000, ..NO_PERIOD }));
        println!("seed {seed}: order {:?}, timeline {:?}", r.final_order, r.leader_timeline);
        assert!(matches!(r.final_order.first(), Some(&2) | Some(&3)), "seed {seed}: {:?}", r.final_order);
    }
}

#[test]
fn steady_load_with_loss_moves_the_leader_only_when_exploring() {
    for seed in 0..seeds() {
        let r = run(SimConfig { drop: 10, ..wan(seed, 180_000, None, NO_PERIOD) });
        println!("seed {seed}: order {:?}, timeline {:?}", r.final_order, r.leader_timeline);
        let settled = r.leader_timeline.iter().find(|(_, l)| *l == US_EAST_REPLICA).map(|(at, _)| *at);
        assert!(settled.is_some(), "seed {seed}: us-east led");
        // After the exploration, us-east keeps the lead (rotation may move it if loss makes
        // it miss eight slots in a row, which these rates do not).
        let last_explore = r.leader_timeline.last().map_or(0, |(at, _)| *at);
        assert_eq!(changes_after(&r, last_explore), 0);
        assert_eq!(r.final_order.first(), Some(&US_EAST_REPLICA), "seed {seed}: {:?}", r.final_order);
        assert!(moves(&r) <= 12, "seed {seed}: one exploration only: {} changes", moves(&r));
    }
}

#[test]
fn a_member_crashed_during_the_exploration_is_never_first() {
    for seed in 0..seeds() {
        // us-east (replica 3) dies at 5 s, before its trial: it is measured as nothing.
        let r =
            run(SimConfig { kill: Some((US_EAST_REPLICA, 5_000)), ..wan(seed, 120_000, None, NO_PERIOD) });
        println!("seed {seed}: order {:?}, timeline {:?}", r.final_order, r.leader_timeline);
        assert_ne!(r.final_order.first(), Some(&US_EAST_REPLICA), "seed {seed}: {:?}", r.final_order);
    }
}

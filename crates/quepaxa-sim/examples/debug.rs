//! Ad-hoc debugging run: `cargo run -p quepaxa-sim --example debug -- <preset> <nodes> <seed>`.
//! Presets: 0 calm, 1 hostile, 2 durability stress.
fn main() {
    let a: Vec<u64> = std::env::args().skip(1).map(|s| s.parse().unwrap()).collect();
    let (nodes, seed) = (a[1] as usize, a[2]);
    let cfg = match a[0] {
        0 => quepaxa_sim::SimConfig {
            drop: 0,
            dup: 0,
            partition: 0,
            crash: 0,
            ..quepaxa_sim::SimConfig::hostile(nodes, seed)
        },
        1 => quepaxa_sim::SimConfig::hostile(nodes, seed),
        _ => quepaxa_sim::SimConfig::durability_stress(nodes, seed),
    };
    match quepaxa_sim::Sim::new(cfg).run() {
        Ok(r) => println!(
            "slots={} entries={} reads={} gap={}ms retained={} installs={}",
            r.slots, r.entries, r.reads, r.max_commit_gap_ms, r.max_retained_slots, r.snapshots_installed
        ),
        Err(e) => println!("{e}"),
    }
}

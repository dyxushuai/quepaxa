//! Ad-hoc debugging run: `cargo run -p quepaxa-sim --example debug -- <nodes> <seed> <hostile:0|1> [absent]`.
fn main() {
    let a: Vec<u64> = std::env::args()
        .skip(1)
        .map(|s| s.parse().unwrap())
        .collect();
    let mut cfg = quepaxa_sim::SimConfig::hostile(a[0] as usize, a[1]);
    if a[2] == 0 {
        cfg.drop = 0;
        cfg.dup = 0;
        cfg.partition = 0;
        cfg.crash = 0;
    }
    cfg.absent = a.get(3).map(|&n| n as usize);
    cfg.heal_budget_ms = 1_500;
    match quepaxa_sim::Sim::new(cfg).run() {
        Ok(r) => println!("{r:?}\n{}", r.trace_tail),
        Err(e) => println!("{e}"),
    }
}

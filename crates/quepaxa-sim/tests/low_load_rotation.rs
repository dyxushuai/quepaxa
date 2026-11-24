//! Regression scenarios for issue #240: a brief hiccup on a fast LAN does not rotate out a
//! healthy leader, and a longer pause that legitimately rotates does not cause a catch-up
//! egress or hedge storm.

use quepaxa_sim::{Sim, SimConfig};

fn fast_lan(seed: u64) -> SimConfig {
    SimConfig {
        drop: 0,
        dup: 0,
        partition: 0,
        crash: 0,
        read: 0,
        max_latency_ms: 1,
        links_ms: None,
        retransmit_ms: 50,
        hedge_max_ms: 1_000,
        max_persist_ms: 1,
        in_flight_slots: 8,
        compact_every: None,
        fault_phase_ms: 1_200,
        heal_budget_ms: 30_000,
        propose: 1_000,
        track_leader: true,
        ..SimConfig::hostile(3, seed)
    }
}

#[test]
fn brief_leader_hiccup_does_not_rotate_a_healthy_leader() {
    for seed in 0..3 {
        let iso = Sim::new(SimConfig { isolate: Some((0, 400, 15)), ..fast_lan(seed) }).run().unwrap();
        assert_eq!(
            iso.final_order[0], 0,
            "seed {seed}: 15 ms pause rotated leader: {:?}",
            iso.leader_timeline
        );
        assert_eq!(iso.metrics[0].stepped_aside, 0, "seed {seed}: false step-aside on 15 ms pause");
        let stall =
            Sim::new(SimConfig { disk_stall: Some((vec![0], 400, 15)), ..fast_lan(seed) }).run().unwrap();
        assert_eq!(
            stall.final_order[0], 0,
            "seed {seed}: 15 ms disk stall rotated leader: {:?}",
            stall.leader_timeline
        );
        assert_eq!(stall.metrics[0].stepped_aside, 0, "seed {seed}: false step-aside on 15 ms disk stall");
    }
}

#[test]
fn rotated_leader_catches_up_without_hedge_or_egress_burst() {
    let base = Sim::new(fast_lan(0)).run().unwrap();
    let r = Sim::new(SimConfig { isolate: Some((0, 400, 100)), ..fast_lan(0) }).run().unwrap();
    assert_eq!(r.final_order[0], 1, "100 ms leader isolation rotates to node 1: {:?}", r.leader_timeline);
    let total_hedged: u64 = r.metrics.iter().map(|m| m.hedged).sum();
    assert!(total_hedged < 80, "handover triggered {total_hedged} hedges: {:?}", r.metrics);
    assert!(
        r.bytes_sent[0] <= base.bytes_sent[0],
        "rotated leader sent {} bytes vs {} baseline",
        r.bytes_sent[0],
        base.bytes_sent[0]
    );
}

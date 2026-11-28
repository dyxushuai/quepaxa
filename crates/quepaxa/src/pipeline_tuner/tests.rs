use super::*;

const MS: u64 = 1_000_000;
const MIB: u32 = 1 << 20;

fn feed_slots(
    tuner: &mut PipelineTuner,
    t: &mut u64,
    count: usize,
    dt_ms: u64,
    weight: u32,
    residence_ms: u64,
    rtt_ms: u64,
) -> Option<u32> {
    let mut last = None;
    for _ in 0..count {
        *t += dt_ms * MS;
        if let Some(w) = tuner.on_slot_committed(weight, residence_ms * MS, rtt_ms * MS, Timestamp(*t)) {
            last = Some(w);
        }
    }
    last
}

#[test]
fn ladder_covers_powers_of_two_and_cap() {
    let (l, n) = build_ladder(8);
    assert_eq!(&l[..n], &[1, 2, 4, 8]);
    let (l, n) = build_ladder(10);
    assert_eq!(&l[..n], &[1, 2, 4, 8, 10]);
    let (l, n) = build_ladder(1);
    assert_eq!(&l[..n], &[1]);
    let (l, n) = build_ladder(64);
    assert_eq!(&l[..n], &[1, 2, 4, 8, 16, 32, 64]);
}

#[test]
fn saturation_steps_up_at_most_once_per_cooldown() {
    let mut tuner = PipelineTuner::new(16, MIB, Timestamp(0));
    assert_eq!(tuner.current(), 1);
    // First saturation steps up immediately.
    assert_eq!(tuner.on_saturated(Timestamp(10 * MS)), Some(2));
    // Same-timestamp burst (e.g., draining a disk stall in one `drive()` call) must NOT multi-step.
    assert_eq!(tuner.on_saturated(Timestamp(10 * MS)), None);
    // Below 50 ms cooldown: suppressed.
    assert_eq!(tuner.on_saturated(Timestamp(59 * MS)), None);
    // Once 50 ms elapses: next rung allowed, even if RTT is much higher on congested WAN.
    assert_eq!(tuner.on_saturated(Timestamp(60 * MS)), Some(4));
    assert_eq!(tuner.on_saturated(Timestamp(110 * MS)), Some(8));
    assert_eq!(tuner.on_saturated(Timestamp(160 * MS)), Some(16));
    assert_eq!(tuner.on_saturated(Timestamp(210 * MS)), None);
    assert_eq!(tuner.current(), 16);
}

#[test]
fn sustained_distress_at_high_w_steps_down_instead_of_trapping() {
    let mut tuner = PipelineTuner::new(16, MIB, Timestamp(0));
    let mut t = 0u64;
    for _ in 0..4 {
        t += 100 * MS;
        let _ = tuner.on_saturated(Timestamp(t));
    }
    assert_eq!(tuner.current(), 16);

    // Feed distressed windows under low demand (w_floor = 1): tuner must step down from 16
    // rather than staying trapped behind `saturation_hold` or failed downward probes.
    for _ in 0..800 {
        t += 6 * MS;
        let _ = tuner.on_distress(Timestamp(t));
        let _ = tuner.on_slot_committed(40 * 1024, 30 * MS, 6 * MS, Timestamp(t));
    }
    assert_eq!(tuner.current(), 1, "sustained distress must back W down to demand floor");
}

#[test]
fn single_rtt_spike_does_not_jump_w_floor() {
    let mut tuner = PipelineTuner::new(16, MIB, Timestamp(0));
    let mut t = 0u64;
    // LAN 4KB steady load: ~40 MiB/s at 6 ms RTT (~240 KiB/rtt < 384 KiB target_fill -> w_floor=1).
    let _ = feed_slots(&mut tuner, &mut t, 120, 6, 250 * 1024, 10, 6);
    assert_eq!(tuner.current(), 1);

    // One 25 ms disk-hiccup window: smoothed RTT + 2-window streak must prevent jumping to W=8.
    let _ = feed_slots(&mut tuner, &mut t, 20, 25, 250 * 1024, 25, 25);
    assert!(tuner.current() <= 2, "single disk spike must not jump W to high rungs, got {}", tuner.current());

    // Return to normal 6 ms RTT: settles back at 1.
    let _ = feed_slots(&mut tuner, &mut t, 200, 6, 250 * 1024, 10, 6);
    assert_eq!(tuner.current(), 1);
}

#[test]
fn stays_at_one_on_lan_when_w2_does_not_improve_latency() {
    let mut tuner = PipelineTuner::new(8, MIB, Timestamp(0));
    let mut t = 0u64;
    for _ in 0..200 {
        t += 7 * MS;
        let res = if tuner.current() == 1 { 12 * MS } else { 15 * MS };
        let _ = tuner.on_slot_committed(64 * 1024, res, 7 * MS, Timestamp(t));
        assert!(tuner.current() <= 2, "must never explore W=4 when W=2 loses");
    }
    assert_eq!(tuner.current(), 1);
}

#[test]
fn climbs_on_wan_when_higher_w_improves_latency_and_rejects_follower_lag_cliff() {
    let mut tuner = PipelineTuner::new(8, MIB, Timestamp(0));
    let mut t = 0u64;
    let mut committed = 0u64;
    let slow_peer = NodeId(3);
    tuner.on_peer_commit(slow_peer, 0, 0, Timestamp(0), 500 * MS);

    let mut decided_times: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
    for _ in 0..400 {
        let w = tuner.current();
        let dt = (85 / u64::from(w)).max(10);
        t += dt * MS;
        tuner.note_decided(Slot(committed), Timestamp(t));
        decided_times.push(t);
        committed += 1;

        let peer_lag_ms = if w >= 8 { 500 } else { 180 };
        let cutoff = t.saturating_sub(peer_lag_ms * MS);
        let peer_c = decided_times.partition_point(|&ts| ts <= cutoff) as u64;
        let _ = tuner.on_peer_commit(slow_peer, peer_c, committed, Timestamp(t), 500 * MS);

        let res_ms = match w {
            1 => 240,
            2 => 175,
            4 => 140,
            _ => 130,
        };
        let _ = tuner.on_slot_committed(40 * 1024, res_ms * MS, 85 * MS, Timestamp(t));
    }
    assert_eq!(tuner.current(), 4, "should settle at W=4 and reject W=8 due to follower lag");
}

#[test]
fn demand_floor_scales_up_and_scales_back_down_when_load_drops() {
    let mut tuner = PipelineTuner::new(16, MIB, Timestamp(0));
    let mut t = 0u64;
    // High-byte load: 640 KiB every 25 ms (~25 MiB/s) at 100 ms RTT -> ~2.5 MiB/rtt -> w_floor=8.
    let _ = feed_slots(&mut tuner, &mut t, 60, 25, 640 * 1024, 150, 100);
    assert!(tuner.current() >= 8, "demand floor must raise W to >= 8, got {}", tuner.current());

    // Load drops sharply to 8 KiB/slot with flat latency -> should scale back down to 1.
    let _ = feed_slots(&mut tuner, &mut t, 400, 30, 8 * 1024, 100, 80);
    assert_eq!(tuner.current(), 1, "should reclaim W down to 1 when load drops");
}

#[test]
fn distress_immediately_aborts_upward_probe() {
    let mut tuner = PipelineTuner::new(8, MIB, Timestamp(0));
    let mut t = 0u64;
    while tuner.current() == 1 {
        t += 20 * MS;
        let _ = tuner.on_slot_committed(16 * 1024, 20 * MS, 10 * MS, Timestamp(t));
    }
    assert_eq!(tuner.current(), 2);
    for _ in 0..2 {
        t += 20 * MS;
        let _ = tuner.on_slot_committed(16 * 1024, 20 * MS, 10 * MS, Timestamp(t));
    }
    assert_eq!(tuner.on_distress(Timestamp(t + MS)), Some(1));
    assert_eq!(tuner.current(), 1);
}

#[test]
fn stays_at_one_on_lan_when_w2_saves_less_than_ten_ms() {
    let mut tuner = PipelineTuner::new(8, MIB, Timestamp(0));
    let mut t = 0u64;
    for _ in 0..400 {
        t += 7 * MS;
        let res = if tuner.current() == 1 { 11 * MS } else { 9 * MS };
        let _ = tuner.on_slot_committed(64 * 1024, res, 7 * MS, Timestamp(t));
        assert!(tuner.current() <= 2, "must not climb past W=2 on sub-10ms LAN gains");
    }
    assert_eq!(tuner.current(), 1);
}

#[test]
fn downward_probe_rejected_when_follower_lag_spikes() {
    let mut tuner = PipelineTuner::new(16, MIB, Timestamp(0));
    let mut t = 0u64;
    let mut committed = 0u64;
    let peer = NodeId(4);
    for _ in 0..3 {
        t += 100 * MS;
        let _ = tuner.on_saturated(Timestamp(t));
    }
    assert_eq!(tuner.current(), 8);
    tuner.on_peer_commit(peer, 0, 0, Timestamp(t), 500 * MS);

    let mut decided_times: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
    for _ in 0..600 {
        let w = tuner.current();
        let dt = (80 / u64::from(w)).max(10);
        t += dt * MS;
        tuner.note_decided(Slot(committed), Timestamp(t));
        decided_times.push(t);
        committed += 1;

        let peer_lag_ms = if w < 8 { 900 } else { 200 };
        let cutoff = t.saturating_sub(peer_lag_ms * MS);
        let peer_c = decided_times.partition_point(|&ts| ts <= cutoff) as u64;
        let _ = tuner.on_peer_commit(peer, peer_c, committed, Timestamp(t), 500 * MS);

        let res_ms = if w < 8 { 140 } else { 135 };
        let _ = tuner.on_slot_committed(40 * 1024, res_ms * MS, 80 * MS, Timestamp(t));
    }
    assert_eq!(tuner.current(), 8, "must reject downward probe to W=4 when follower lag spikes");
}

#[test]
fn reset_anchors_at_clamped_value() {
    let mut tuner = PipelineTuner::new(8, MIB, Timestamp(0));
    tuner.reset(4, Timestamp(100 * MS));
    assert_eq!(tuner.current(), 4);
    tuner.reset(0, Timestamp(200 * MS));
    assert_eq!(tuner.current(), 1);
    tuner.reset(99, Timestamp(300 * MS));
    assert_eq!(tuner.current(), 8);
}

#[test]
fn overload_rtt_inflation_does_not_lock_w_floor_and_suppresses_saturation() {
    let mut tuner = PipelineTuner::new(16, MIB, Timestamp(0));
    let mut t = 0u64;
    // Ramp to W=16 and complete clean windows at 2 ms LAN RTT so propagation min_rtt = 2 ms.
    for _ in 0..4 {
        t += 60 * MS;
        let _ = tuner.on_saturated(Timestamp(t));
    }
    assert_eq!(tuner.current(), 16);
    for _ in 0..160 {
        t += 10 * MS;
        let _ = tuner.on_slot_committed(200 * 1024, 3 * MS, 2 * MS, Timestamp(t));
    }
    assert_eq!(tuner.current(), 16);

    // Under overload, RTT inflates 50x to 100 ms alongside distress (retransmits/lost slots).
    // With 600 KiB every 20 ms (= 30 MiB/s), inflated 100 ms RTT would compute bpr = 3 MiB
    // (w_floor = 16), locking W=16; propagation min_rtt = 2 ms keeps w_floor = 1 so sustained
    // distress steps W down directly.
    let mut stepped_down = false;
    for _ in 0..250 {
        t += 20 * MS;
        let _ = tuner.on_distress(Timestamp(t));
        if let Some(w) = tuner.on_slot_committed(600 * 1024, 120 * MS, 100 * MS, Timestamp(t))
            && w < 16
        {
            stepped_down = true;
            break;
        }
    }
    assert!(stepped_down && tuner.current() == 8, "sustained distress must step W down: {}", tuner.current());

    // Immediately after stepping down to W=8 (< max_w), on_saturated must remain suppressed
    // during distress_hold so W cannot bounce straight back to 16 while overload persists.
    t += 60 * MS;
    assert_eq!(
        tuner.on_saturated(Timestamp(t)),
        None,
        "on_saturated must stay suppressed after distress backoff"
    );
    assert_eq!(tuner.current(), 8);

    // Even a single distress event in the current window suppresses on_saturated at W < max_w.
    let mut mid = PipelineTuner::new(16, MIB, Timestamp(0));
    assert_eq!(mid.on_saturated(Timestamp(60 * MS)), Some(2));
    let _ = mid.on_distress(Timestamp(120 * MS));
    assert_eq!(
        mid.on_saturated(Timestamp(180 * MS)),
        None,
        "on_saturated must stay suppressed on active distress"
    );
    assert_eq!(mid.current(), 2);

    // Once distress_hold clears after clean windows, on_saturated works again.
    for _ in 0..200 {
        t += 20 * MS;
        let _ = tuner.on_slot_committed(50 * 1024, 3 * MS, 2 * MS, Timestamp(t));
    }
    let before = tuner.current();
    assert!(before < 16);
    t += 60 * MS;
    assert!(
        tuner.on_saturated(Timestamp(t)).is_some(),
        "on_saturated resumes once clean windows clear distress"
    );
}

#[test]
fn arbitrary_event_sequences_keep_bounds_and_never_panic() {
    use proptest::prelude::*;
    proptest!(|(
        max_w in 1u32..=64,
        max_batch in 64u32..=(2 << 20),
        events in proptest::collection::vec((0u8..5, 0u64..500, 0u32..=1_048_576, 0u32..=80), 1..200)
    )| {
        let mut tuner = PipelineTuner::new(max_w, max_batch, Timestamp(0));
        let mut now = 0u64;
        let mut committed = 0u64;
        for (kind, dt_ms, arg, peer) in events {
            now = now.saturating_add(dt_ms * MS);
            let changed = match kind {
                0 => {
                    tuner.note_decided(Slot(committed), Timestamp(now));
                    committed += 1;
                    tuner.on_slot_committed(arg, dt_ms * MS, (dt_ms / 2 + 1) * MS, Timestamp(now))
                }
                1 => tuner.on_saturated(Timestamp(now)),
                2 => tuner.on_distress(Timestamp(now)),
                3 => {
                    let peer_c = committed.saturating_sub(u64::from(arg % 80));
                    tuner.on_peer_commit(NodeId(peer % 5), peer_c, committed, Timestamp(now), 200 * MS)
                }
                _ => {
                    tuner.reset(peer, Timestamp(now));
                    None
                }
            };
            if let Some(w) = changed {
                prop_assert!(w >= 1 && w <= max_w);
            }
            prop_assert!(tuner.current() >= 1 && tuner.current() <= max_w);
        }
    });
}

//! Leader placement decisions, from injected round trips and write counts.

use super::placement::PROBE_INTERVAL;
use super::*;

const MS: u64 = 1_000_000;
const SUSTAIN: u64 = 180 * PROBE_INTERVAL;
const HOLD: u64 = 600 * PROBE_INTERVAL;
/// A round trip a member does not report.
const UNKNOWN: u64 = u64::MAX;

/// Round trips in ms: us-east, us-west, eu-west, ap-ne, sa-east (`wan_leaders`).
const WAN: [[u64; 5]; 5] = [
    [0, 70, 80, 180, 120],
    [70, 0, 140, 110, 170],
    [80, 140, 0, 230, 190],
    [180, 110, 230, 0, 300],
    [120, 170, 190, 300, 0],
];

/// Replica `me` of `members`, leading with `order`, placement on.
fn leader(me: u32, order: &[u32]) -> Replica {
    let members: Vec<NodeId> = order.iter().copied().map(NodeId).collect();
    let mut cfg = Config::new([3; 16], NodeId(me), members.clone(), [9; 32]);
    cfg.leader_order = members;
    cfg.placement = true;
    let mut r = Replica::new(cfg, [], Timestamp(0)).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    r.handle(Timestamp(0), Input::Persisted(PersistId(0))).unwrap();
    r.poll_actions(&mut out);
    r
}

/// One probe round at `t`: every member reports `rtt` (ms, by id) fresh, and `writes`
/// application entries per origin were applied since the last round.
fn round(r: &mut Replica, t: u64, rtt: &[&[u64]], writes: &[(u32, u64)]) {
    let now = Timestamp(t);
    let me = r.me.0 as usize;
    for (a, row) in rtt.iter().enumerate() {
        let table: Vec<(NodeId, u64)> = row
            .iter()
            .enumerate()
            .filter(|(b, ms)| *b != a && **ms != UNKNOWN)
            .map(|(b, ms)| (NodeId(b as u32), ms * MS))
            .collect();
        if a == me {
            for (n, v) in table {
                r.placement.rtt.insert(n, (v, now));
            }
        } else {
            r.placement.tables.insert(NodeId(a as u32), (table, now));
        }
    }
    r.placement.writes = writes.iter().map(|&(o, n)| (NodeId(o), n)).collect();
    r.placement.last_write = Some(now);
    r.placement.next_probe = Some(now);
    r.handle_timeout(now).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
}

/// Rounds once a second from `from` for `secs` seconds; the time after the last one.
fn rounds(r: &mut Replica, from: u64, secs: u64, rtt: &[&[u64]], writes: &[(u32, u64)]) -> u64 {
    let mut t = from;
    for _ in 0..secs {
        round(r, t, rtt, writes);
        t += PROBE_INTERVAL;
    }
    t
}

/// The order placement submitted, if any.
fn moved(r: &Replica) -> Option<Vec<u32>> {
    let id = r.placement.moving?;
    let item =
        r.pending.iter().find(|i| i.entry.id == id).map(|i| i.entry.payload.clone()).or_else(|| {
            r.batches.values().flat_map(|b| batch::changes(&b.value)).find(|(k, _)| k.2 == id).map(|(_, p)| p)
        })?;
    let (_, order) = crate::schedule::decode_change(&item)?;
    Some(order.into_iter().map(|n| n.0).collect())
}

fn wan() -> Vec<&'static [u64]> {
    WAN.iter().map(|r| &r[..]).collect()
}

fn three(x: u64) -> [[u64; 3]; 3] {
    [[0, 100, 100], [100, 0, x], [100, x, 0]]
}

fn rows(m: &[[u64; 3]; 3]) -> Vec<&[u64]> {
    m.iter().map(|r| &r[..]).collect()
}

#[test]
fn the_leader_moves_to_the_best_placed_member_after_the_sustained_period() {
    // ap-ne leads; everyone writes equally: us-east is best placed.
    let mut r = leader(3, &[3, 0, 1, 2, 4]);
    let all = [(0, 10), (1, 10), (2, 10), (3, 10), (4, 10)];
    let t = rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL, &wan(), &all);
    assert_eq!(moved(&r), None, "not before the sustained period");
    round(&mut r, t, &wan(), &all);
    assert_eq!(moved(&r), Some(alloc::vec![0, 1, 2, 4, 3]));
    // One move at a time.
    let id = r.placement.moving;
    rounds(&mut r, t + PROBE_INTERVAL, 5, &wan(), &all);
    assert_eq!(r.placement.moving, id);
    assert_eq!(r.outstanding.values().filter(|(_, k, _)| *k == Kind::Reconfigure).count(), 1);
}

#[test]
fn a_gain_below_twenty_percent_never_moves_the_leader() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    rounds(&mut r, PROBE_INTERVAL, 2 * SUSTAIN / PROBE_INTERVAL, &rows(&three(72)), &writes);
    assert_eq!(moved(&r), None);
    let mut r = leader(0, &[0, 1, 2]);
    rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL + 1, &rows(&three(70)), &writes);
    assert_eq!(moved(&r), Some(alloc::vec![1, 0, 2]));
}

#[test]
fn a_round_without_the_gain_restarts_the_period() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    let t = rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL - 10, &rows(&three(50)), &writes);
    round(&mut r, t, &rows(&three(100)), &writes);
    let t = rounds(&mut r, t + PROBE_INTERVAL, 20, &rows(&three(50)), &writes);
    assert_eq!(moved(&r), None, "the period started over");
    rounds(&mut r, t, SUSTAIN / PROBE_INTERVAL, &rows(&three(50)), &writes);
    assert!(moved(&r).is_some());
}

#[test]
fn bursts_separated_by_silence_never_add_up() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    let half = SUSTAIN / PROBE_INTERVAL / 2 + 5;
    let t = rounds(&mut r, PROBE_INTERVAL, half, &rows(&three(50)), &writes);
    rounds(&mut r, t + 60 * PROBE_INTERVAL, half, &rows(&three(50)), &writes);
    assert_eq!(moved(&r), None);
}

#[test]
fn too_few_writes_move_nothing() {
    let mut r = leader(0, &[0, 1, 2]);
    rounds(&mut r, PROBE_INTERVAL, 2 * SUSTAIN / PROBE_INTERVAL, &rows(&three(50)), &[(1, 0)]);
    assert_eq!(moved(&r), None);
}

#[test]
fn a_reorder_behind_the_leader_proposes_nothing() {
    // The leader is already best placed; only the others would swap.
    let writes = [(0, 10)];
    let mut r = leader(0, &[0, 2, 1]);
    rounds(
        &mut r,
        PROBE_INTERVAL,
        2 * SUSTAIN / PROBE_INTERVAL,
        &rows(&[[0, 50, 100], [50, 0, 70], [100, 70, 0]]),
        &writes,
    );
    assert_eq!(moved(&r), None);
}

#[test]
fn a_candidate_a_writer_cannot_reach_is_skipped() {
    // us-east is best placed, but neither it nor sa-east (writing) reports a round trip
    // between them: us-west is placed instead.
    let mut r = leader(3, &[3, 0, 1, 2, 4]);
    let mut m = WAN;
    (m[4][0], m[0][4]) = (UNKNOWN, UNKNOWN);
    let m: Vec<&[u64]> = m.iter().map(|r| &r[..]).collect();
    let all = [(0, 10), (1, 10), (2, 10), (3, 10), (4, 10)];
    rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL + 1, &m, &all);
    assert_eq!(moved(&r).map(|o| o[0]), Some(1));
}

#[test]
fn without_a_majority_of_round_trips_nothing_moves() {
    // Nobody reports a round trip to the leader: it has no cost, and nothing is proposed.
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(1, &[1, 0, 2]);
    let m = [[0, UNKNOWN, 50], [UNKNOWN, 0, UNKNOWN], [50, UNKNOWN, 0]];
    rounds(&mut r, PROBE_INTERVAL, 2 * SUSTAIN / PROBE_INTERVAL, &rows(&m), &writes);
    assert_eq!(moved(&r), None);
}

#[test]
fn only_the_leader_in_force_evaluates() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(1, &[0, 1, 2]);
    rounds(&mut r, PROBE_INTERVAL, 2 * SUSTAIN / PROBE_INTERVAL, &rows(&three(50)), &writes);
    assert_eq!(moved(&r), None);
    // Stepped aside when the move would be due.
    let mut r = leader(0, &[0, 1, 2]);
    let t = rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL, &rows(&three(50)), &writes);
    assert!(r.placement.target.is_some());
    r.now = Timestamp(t);
    r.watch.aside = true;
    r.evaluate_placement();
    assert_eq!((moved(&r), r.placement.target), (None, None));
}

#[test]
fn a_change_on_its_way_holds_placement_back() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    let change = crate::schedule::encode_change(0, &[NodeId(2), NodeId(0), NodeId(1)]);
    r.relayed.push_back(Item {
        entry: Entry { origin: NodeId(2), incarnation: 1, id: ProposalId(0), payload: change },
        kind: Kind::Reconfigure,
    });
    rounds(&mut r, PROBE_INTERVAL, 2 * SUSTAIN / PROBE_INTERVAL, &rows(&three(50)), &writes);
    assert_eq!(r.placement.moving, None);
}

#[test]
fn a_recent_order_change_holds_placement_back() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    // The order changed five minutes before the gain appears; the hold is ten.
    r.placement.seen_order = alloc::vec![NodeId(0), NodeId(1), NodeId(2)];
    r.placement.changed_at = Some(Timestamp(PROBE_INTERVAL));
    let t = rounds(&mut r, 300 * PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL + 10, &rows(&three(50)), &writes);
    assert_eq!(moved(&r), None, "within the hold");
    let until = PROBE_INTERVAL + HOLD;
    rounds(&mut r, t, (until - t) / PROBE_INTERVAL + 2, &rows(&three(50)), &writes);
    assert!(moved(&r).is_some(), "after the hold");
}

#[test]
fn a_rotated_out_leader_is_not_placed_first_and_twice_as_long_the_next_time() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    round(&mut r, PROBE_INTERVAL, &rows(&three(50)), &writes);
    // Rotation demotes 1 (then leading) to last.
    r.placement.seen_order = alloc::vec![NodeId(1), NodeId(2), NodeId(0)];
    r.placement.led_since = Timestamp(PROBE_INTERVAL);
    r.applied_schedule = Schedule::with_order(alloc::vec![NodeId(2), NodeId(0), NodeId(1)]);
    round(&mut r, 2 * PROBE_INTERVAL, &rows(&three(50)), &writes);
    let e = r.placement.excluded[&NodeId(1)];
    assert_eq!((e.factor, e.until), (1, Timestamp(2 * PROBE_INTERVAL + HOLD)));
    // Restored and rotated out again before it led for a whole hold: twice as long.
    r.placement.seen_order = alloc::vec![NodeId(1), NodeId(2), NodeId(0)];
    round(&mut r, 3 * PROBE_INTERVAL, &rows(&three(50)), &writes);
    let e = r.placement.excluded[&NodeId(1)];
    assert_eq!((e.factor, e.until), (2, Timestamp(3 * PROBE_INTERVAL + 2 * HOLD)));
}

#[test]
fn an_excluded_member_is_not_placed_first() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    let until = Timestamp(10 * HOLD);
    r.placement.excluded.insert(NodeId(1), placement::Exclusion { until, factor: 1 });
    rounds(&mut r, PROBE_INTERVAL, 2 * SUSTAIN / PROBE_INTERVAL, &rows(&three(50)), &writes);
    assert_eq!(moved(&r), None);
}

#[test]
fn the_move_never_reaches_the_application() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL + 1, &rows(&three(50)), &writes);
    let id = r.placement.moving.expect("a move");
    assert_eq!(
        r.propose_reconfiguration(r.now, alloc::vec![NodeId(0), NodeId(1), NodeId(2)], Vec::new()),
        Err(Error::ReconfigurationPending)
    );
    // Decided: applied, but neither committed nor dropped to the application.
    let (key, value) = r
        .batches
        .values()
        .find(|b| !batch::changes(&b.value).is_empty())
        .map(|b| (b.key, b.value.clone()))
        .unwrap();
    let slot = Slot(r.committed);
    let decided = Proposal { priority: 1, key, value };
    r.handle(r.now, tests::from_peer(1, 0, Body::Decided { slot, proposal: decided })).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    assert!(out.iter().any(|a| matches!(a, Action::Reconfigured { .. })), "{out:?}");
    assert!(
        !out.iter().any(
            |a| matches!(a, Action::Committed { id: c, .. } | Action::Dropped { id: c, .. } if *c == id)
        )
    );
    assert_eq!(r.placement.moving, None);
    assert!(!r.outstanding.contains_key(&id));
}

#[test]
fn a_membership_change_without_an_order_keeps_the_leader() {
    let mut r = leader(2, &[2, 0, 1]);
    r.propose_reconfiguration(r.now, alloc::vec![NodeId(0), NodeId(1), NodeId(2), NodeId(3)], Vec::new())
        .unwrap();
    let item = r
        .pending
        .iter()
        .chain(&r.relayed)
        .find(|i| i.kind == Kind::Reconfigure)
        .map(|i| i.entry.payload.clone());
    let item = item
        .or_else(|| r.batches.values().flat_map(|b| batch::changes(&b.value)).map(|(_, p)| p).next())
        .unwrap();
    let (_, order) = crate::schedule::decode_change(&item).unwrap();
    assert_eq!(order, [NodeId(2), NodeId(0), NodeId(1), NodeId(3)]);
}

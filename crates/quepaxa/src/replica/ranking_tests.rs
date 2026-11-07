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
        } else if !table.is_empty() {
            // A member that reports nothing has sent no table.
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
    r.placement.changed_at = Some(Timestamp(PROBE_INTERVAL));
    let t = rounds(&mut r, 300 * PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL + 10, &rows(&three(50)), &writes);
    assert_eq!(moved(&r), None, "within the hold");
    let until = PROBE_INTERVAL + HOLD;
    rounds(&mut r, t, (until - t) / PROBE_INTERVAL + 2, &rows(&three(50)), &writes);
    assert!(moved(&r).is_some(), "after the hold");
}

#[test]
fn a_rotated_out_leader_is_not_placed_first_and_twice_as_long_the_next_time() {
    let mut r = leader(0, &[0, 1, 2]);
    // Slots 0-7 decided by others count a rotation from slot 7 + LAG: 0 is demoted.
    let mut s = Schedule::with_order(alloc::vec![NodeId(0), NodeId(1), NodeId(2)]);
    for slot in 0..8 {
        s.observe(Slot(slot), NodeId(1));
    }
    r.applied_schedule = s.clone();
    r.now = Timestamp(PROBE_INTERVAL);
    let first = 7 + crate::schedule::LAG;
    r.note_applied(Slot(first - 1));
    let e = r.placement.excluded[&NodeId(0)];
    assert_eq!((e.factor, e.until), (1, Timestamp(PROBE_INTERVAL + HOLD)));
    assert_eq!(r.placement.changed_at, Some(Timestamp(PROBE_INTERVAL)));
    // Restored by a change and rotated out again before it led for a whole hold.
    let back = first;
    s.reconfigure(Slot(back), s.generation(), alloc::vec![NodeId(0), NodeId(1), NodeId(2)]).unwrap();
    let from = back + crate::schedule::LAG;
    for slot in from..from + 8 {
        s.observe(Slot(slot), NodeId(1));
    }
    r.applied_schedule = s;
    r.now = Timestamp(3 * PROBE_INTERVAL);
    r.note_applied(Slot(from - 1));
    assert_eq!(r.placement.excluded[&NodeId(0)].factor, 1, "a decided change is no demotion");
    r.now = Timestamp(4 * PROBE_INTERVAL);
    r.note_applied(Slot(from + 7 + crate::schedule::LAG - 1));
    let e = r.placement.excluded[&NodeId(0)];
    assert_eq!((e.factor, e.until), (2, Timestamp(4 * PROBE_INTERVAL + 2 * HOLD)));
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

#[test]
fn placements_own_move_is_no_demotion() {
    let mut r = leader(3, &[3, 0, 1, 2, 4]);
    let mut s = r.applied_schedule.clone();
    s.reconfigure(Slot(0), 0, alloc::vec![NodeId(0), NodeId(1), NodeId(2), NodeId(4), NodeId(3)]).unwrap();
    r.applied_schedule = s;
    r.now = Timestamp(PROBE_INTERVAL);
    r.note_applied(Slot(crate::schedule::LAG - 1));
    assert!(r.placement.excluded.is_empty());
    assert_eq!(r.placement.changed_at, Some(Timestamp(PROBE_INTERVAL)));
}

#[test]
fn a_rotation_not_yet_in_force_holds_placement_back() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    // A rotation is counted but takes effect only after the move would be due.
    for slot in 0..8 {
        r.schedule.observe(Slot(slot), NodeId(1));
    }
    rounds(&mut r, PROBE_INTERVAL, 2 * SUSTAIN / PROBE_INTERVAL, &rows(&three(50)), &writes);
    assert_eq!(r.placement.moving, None);
}

#[test]
fn a_writer_that_stopped_decays_to_nothing() {
    let mut r = leader(0, &[0, 1, 2]);
    let t = rounds(&mut r, PROBE_INTERVAL, 20, &rows(&three(50)), &[(1, 10), (2, 1)]);
    assert!(r.placement.rates.contains_key(&NodeId(2)));
    rounds(&mut r, t, 60, &rows(&three(50)), &[(1, 10)]);
    assert!(!r.placement.rates.contains_key(&NodeId(2)), "{:?}", r.placement.rates);
}

#[test]
fn a_fenced_or_snapshot_resolved_move_is_never_reported() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL + 1, &rows(&three(50)), &writes);
    let id = r.placement.moving.expect("a move");
    r.fence();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    assert!(!out.iter().any(|a| matches!(a, Action::Dropped { id: d, .. } if *d == id)), "{out:?}");
    assert_eq!(r.placement.moving, None);

    let mut r = leader(0, &[0, 1, 2]);
    rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL + 1, &rows(&three(50)), &writes);
    let id = r.placement.moving.expect("a move");
    let mut dedup = crate::dedup::Dedup::default();
    dedup.first_time(NodeId(0), r.incarnation, id);
    let members = [NodeId(0), NodeId(1), NodeId(2)];
    let state = CoreState {
        next: 10,
        leaders: members.to_vec(),
        schedule: Schedule::with_order(members.to_vec()),
        dedup,
    }
    .encode();
    assert_eq!(r.install_snapshot(r.now, Slot(9), state), Ok(true));
    r.poll_actions(&mut out);
    assert!(
        !out.iter().any(
            |a| matches!(a, Action::Committed { id: c, .. } | Action::Dropped { id: c, .. } if *c == id)
        ),
        "{out:?}"
    );
    assert_eq!(r.placement.moving, None);
}

#[test]
fn the_best_member_is_placed_even_if_another_started_the_period() {
    // us-east sends no table at first, so it cannot be ranked and us-west starts the period;
    // once us-east is rankable it ranks best, and it is the one placed.
    let mut r = leader(3, &[3, 0, 1, 2, 4]);
    let all = [(0, 10), (1, 10), (2, 10), (3, 10), (4, 10)];
    let mut silent = WAN;
    silent[0] = [0, UNKNOWN, UNKNOWN, UNKNOWN, UNKNOWN];
    let silent: Vec<&[u64]> = silent.iter().map(|r| &r[..]).collect();
    let t = rounds(&mut r, PROBE_INTERVAL, 5, &silent, &all);
    assert_eq!(r.placement.target.map(|t| t.member), Some(NodeId(1)));
    rounds(&mut r, t, SUSTAIN / PROBE_INTERVAL, &wan(), &all);
    assert_eq!(moved(&r).map(|o| o[0]), Some(0));
}

#[test]
fn one_write_a_round_reaches_the_floor() {
    let mut r = leader(0, &[0, 1, 2]);
    rounds(&mut r, PROBE_INTERVAL, 60, &rows(&three(50)), &[(1, 1)]);
    assert_eq!(r.placement.rates[&NodeId(1)], 8);
||||||| parent of 039c11a (feat(replica): expose where the leader is and why)

fn a_move_that_takes_effect_advances_the_epoch_and_counts() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL + 1, &rows(&three(50)), &writes);
    assert_eq!((r.metrics().placement_moves, r.metrics().epoch, r.metrics().leader_position), (1, 0, 0));
    let (key, value) = r
        .batches
        .values()
        .find(|b| !batch::changes(&b.value).is_empty())
        .map(|b| (b.key, b.value.clone()))
        .unwrap();
    let first = r.committed;
    let mut proposals = alloc::vec![Proposal { priority: 1, key, value }];
    proposals.extend((1..=crate::schedule::LAG).map(|s| tests::proposal(1, 1, s)));
    r.handle(r.now, tests::from_peer(1, 0, Body::Entries { first: Slot(first), proposals })).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    // In force from the decision slot + LAG: member 1 leads epoch 1, this replica (the most
    // costly) is last.
    assert_eq!((r.metrics().placement_moves, r.metrics().epoch, r.metrics().leader_position), (1, 1, 2));
    assert_eq!(r.members().iter().map(|m| m.id.0).collect::<Vec<_>>(), [1, 2, 0]);
}

#[test]
fn the_member_view_reports_what_each_peer_said_last() {
    let mut r = leader(0, &[0, 1, 2]);
    round(&mut r, PROBE_INTERVAL, &rows(&three(50)), &[(0, 1)]);
    let env =
        Envelope { cluster_id: [3; 16], config_epoch: 0, from: NodeId(1), to: NodeId(0), commit_index: 9 };
    let msg = Message { env, body: Body::CatchUp { from: Slot(0) } }.encode();
    r.handle(Timestamp(2 * PROBE_INTERVAL), Input::Message(msg)).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    let view = r.members();
    assert_eq!(
        view[0],
        MemberView { id: NodeId(0), round_trip: None, committed: Some(r.committed), last_heard: None }
    );
    assert_eq!(view[1].committed, Some(9));
    assert_eq!(view[1].last_heard, Some(Timestamp(2 * PROBE_INTERVAL)));
    assert_eq!(view[1].round_trip, Some(100 * MS));
    assert_eq!(view[2].committed, None);
}

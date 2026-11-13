//! Order and membership changes: holds, demotions, and how a move takes effect.

use super::*;

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
    assert_eq!(r.placement.policy.moving(), None);
}

#[test]
fn a_recent_order_change_holds_placement_back() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    // The order changed five minutes before the gain appears; the hold is ten.
    r.placement.policy.on_order_applied(Timestamp(PROBE_INTERVAL), HOLD, NodeId(0), NodeId(0), true);
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
    let e = r.placement.policy.exclusion(NodeId(0)).unwrap();
    assert_eq!((e.factor, e.until), (1, Timestamp(PROBE_INTERVAL + HOLD)));
    assert_eq!(r.placement.policy.changed_at(), Some(Timestamp(PROBE_INTERVAL)));
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
    assert_eq!(r.placement.policy.exclusion(NodeId(0)).unwrap().factor, 1, "a decided change is no demotion");
    r.now = Timestamp(4 * PROBE_INTERVAL);
    r.note_applied(Slot(from + 7 + crate::schedule::LAG - 1));
    let e = r.placement.policy.exclusion(NodeId(0)).unwrap();
    assert_eq!((e.factor, e.until), (2, Timestamp(4 * PROBE_INTERVAL + 2 * HOLD)));
}

#[test]
fn the_move_never_reaches_the_application() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL + 1, &rows(&three(50)), &writes);
    let id = r.placement.policy.moving().expect("a move");
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
    assert_eq!(r.placement.policy.moving(), None);
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
    assert!((0..5).all(|n| r.placement.policy.exclusion(NodeId(n)).is_none()));
    assert_eq!(r.placement.policy.changed_at(), Some(Timestamp(PROBE_INTERVAL)));
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
    assert_eq!(r.placement.policy.moving(), None);
}

#[test]
fn a_fenced_or_snapshot_resolved_move_is_never_reported() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL + 1, &rows(&three(50)), &writes);
    let id = r.placement.policy.moving().expect("a move");
    r.fence();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    assert!(!out.iter().any(|a| matches!(a, Action::Dropped { id: d, .. } if *d == id)), "{out:?}");
    assert_eq!(r.placement.policy.moving(), None);

    let mut r = leader(0, &[0, 1, 2]);
    rounds(&mut r, PROBE_INTERVAL, SUSTAIN / PROBE_INTERVAL + 1, &rows(&three(50)), &writes);
    let id = r.placement.policy.moving().expect("a move");
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
    assert_eq!(r.placement.policy.moving(), None);
}

#[test]
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
    let decided = alloc::vec![Proposal { priority: 1, key, value }];
    r.handle(r.now, tests::from_peer(1, 0, Body::Entries { first: Slot(first), proposals: decided }))
        .unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    // Decided, not yet in force: the gauges still show the epoch in force.
    assert_eq!((r.metrics().placement_moves, r.metrics().epoch, r.metrics().leader_position), (1, 0, 0));
    let proposals = (1..=crate::schedule::LAG).map(|s| tests::proposal(1, 1, s)).collect();
    r.handle(r.now, tests::from_peer(1, 0, Body::Entries { first: Slot(first + 1), proposals })).unwrap();
    r.poll_actions(&mut out);
    // In force from the decision slot + LAG: member 1 leads epoch 1, this replica (the most
    // costly) is last.
    assert_eq!((r.metrics().placement_moves, r.metrics().epoch, r.metrics().leader_position), (1, 1, 2));
    assert_eq!(r.members().iter().map(|m| m.id.0).collect::<Vec<_>>(), [1, 2, 0]);
}

#[test]
fn a_leader_that_led_for_exactly_a_whole_hold_is_no_longer_excluded() {
    let mut r = leader(0, &[0, 1, 2]);
    r.placement.policy.on_restart(Timestamp(PROBE_INTERVAL), NodeId(0), false);
    let until = Timestamp(10 * HOLD);
    r.placement.policy.insert_exclusion(NodeId(0), Exclusion { until, factor: 4 });
    r.now = Timestamp(PROBE_INTERVAL + HOLD - 1);
    r.close_round();
    assert!(r.placement.policy.exclusion(NodeId(0)).is_some(), "not yet a whole hold");
    r.now = Timestamp(PROBE_INTERVAL + HOLD);
    r.close_round();
    assert!(r.placement.policy.exclusion(NodeId(0)).is_none());
}

#[test]
fn a_snapshot_after_an_order_change_restarts_the_hold() {
    let members = [NodeId(0), NodeId(1), NodeId(2)];
    let target = Target { member: NodeId(1), leader: NodeId(0), since: Timestamp(1) };
    let install = |schedule: Schedule| {
        let mut r = leader(0, &[0, 1, 2]);
        // Member 1 gains over 0 from time 1 on.
        let periods = Periods { sustain: SUSTAIN, hold: HOLD };
        let costs = [Some(1_000 * u128::from(MS)), Some(u128::from(MS)), None];
        r.placement.policy.decide(Timestamp(1), periods, WRITE_FLOOR, Some((&members, &costs)));
        assert_eq!(r.placement.policy.target(), Some(target));
        let state = CoreState { next: 10, leaders: members.to_vec(), schedule, dedup: Default::default() };
        assert_eq!(r.install_snapshot(Timestamp(PROBE_INTERVAL), Slot(9), state.encode()), Ok(true));
        (r.placement.policy.changed_at(), r.placement.policy.target())
    };
    let mut changed = Schedule::with_order(members.to_vec());
    changed.reconfigure(Slot(0), 0, alloc::vec![NodeId(1), NodeId(0), NodeId(2)]).unwrap();
    assert_eq!(install(changed), (Some(Timestamp(PROBE_INTERVAL)), None));
    assert_eq!(install(Schedule::with_order(members.to_vec())), (None, Some(target)), "nothing changed");
}

#[test]
fn the_hold_ends_exactly_a_hold_after_the_last_order_change() {
    let writes = [(0, 10), (1, 10)];
    let mut r = leader(0, &[0, 1, 2]);
    r.placement.policy.on_order_applied(Timestamp(PROBE_INTERVAL), HOLD, NodeId(0), NodeId(0), true);
    let t = rounds(&mut r, PROBE_INTERVAL, HOLD / PROBE_INTERVAL, &rows(&three(50)), &writes);
    assert_eq!(moved(&r), None, "within the hold");
    assert_eq!(t, PROBE_INTERVAL + HOLD);
    round(&mut r, t, &rows(&three(50)), &writes);
    assert_eq!(moved(&r).map(|o| o[0]), Some(1));
}

#[test]
fn a_membership_change_without_an_order_puts_the_best_placed_member_first() {
    // ap-ne leads; us-east (0) is best placed: removing sa-east (4) also reorders.
    let mut r = leader(3, &[3, 0, 1, 2, 4]);
    let all = [(0, 10), (1, 10), (2, 10), (3, 10), (4, 10)];
    round(&mut r, PROBE_INTERVAL, &wan(), &all);
    let in_force: Vec<NodeId> = [3, 0, 1, 2, 4].map(NodeId).to_vec();
    let members: Vec<NodeId> = [0, 1, 2, 3, 5].map(NodeId).to_vec();
    let order: Vec<u32> = r.placed_order(&in_force, &members).into_iter().map(|n| n.0).collect();
    assert_eq!(order[0], 0, "{order:?}");
    assert_eq!(order.last(), Some(&5), "the new member goes last: {order:?}");
}

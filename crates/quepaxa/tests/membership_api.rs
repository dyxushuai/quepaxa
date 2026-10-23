//! Membership changes, retirement and restarts across replicas, through the public API.

mod common;

use bytes::Bytes;
use common::{T0, config, drain, ordered, settle};
use quepaxa::{Action, Error, Input, NodeId, Replica, Timestamp};

/// Like [`run_cluster`], but replicas in `down` neither send nor receive.
fn run_without(rs: &mut [Replica], down: &[usize]) -> Vec<(usize, Action)> {
    let mut queue = std::collections::VecDeque::new();
    let mut seen = Vec::new();
    loop {
        for (i, r) in rs.iter_mut().enumerate() {
            for a in drain(r) {
                match a {
                    Action::Send(o) => queue.push_back((o.to.0 as usize, Input::Message(o.bytes))),
                    Action::Persist { id, then, .. } => {
                        queue.extend(then.into_iter().map(|o| (o.to.0 as usize, Input::Message(o.bytes))));
                        queue.push_back((i, Input::Persisted(id)));
                    }
                    other => seen.push((i, other)),
                }
            }
        }
        let Some((to, input)) = queue.pop_front() else { return seen };
        if down.contains(&to) && !matches!(input, Input::Persisted(_)) {
            continue;
        }
        rs[to].handle(T0, input).unwrap();
    }
}

#[test]
fn a_member_is_replaced_and_the_new_majority_commits_without_it() {
    // Replica 3 joins with the original configuration; replica 0 leaves.
    let mut rs = [ordered(0, &[]), ordered(1, &[]), ordered(2, &[]), ordered(3, &[])];
    let mut seen = run_without(&mut rs, &[]);
    let id = rs[0].propose_reconfiguration(T0, vec![NodeId(1), NodeId(2), NodeId(3)], vec![]).unwrap();
    // One change at a time.
    assert_eq!(
        rs[0].propose_reconfiguration(T0, vec![NodeId(0), NodeId(1), NodeId(2)], vec![]),
        Err(Error::ReconfigurationPending)
    );
    seen.extend(run_without(&mut rs, &[]));
    assert!(seen.iter().any(|(i, a)| *i == 0 && matches!(a, Action::Committed { id: c, .. } if *c == id)));
    // Writes carry on through the window (which also fills by itself).
    for k in 0..70u32 {
        rs[1].propose(T0, Bytes::from(format!("w{k}"))).unwrap();
        seen.extend(run_without(&mut rs, &[]));
    }
    let reconfigured: Vec<usize> = seen
        .iter()
        .filter_map(|(i, a)| match a {
            Action::Reconfigured { epoch: 1, members, .. }
                if members == &[NodeId(1), NodeId(2), NodeId(3)] =>
            {
                Some(*i)
            }
            _ => None,
        })
        .collect();
    assert_eq!(reconfigured, [0, 1, 2, 3], "every replica, the joiner included, applies the change once");
    // The removed replica asks the new members for their progress on its retransmit timer.
    for r in &mut rs {
        r.handle_timeout(Timestamp(1_000_000_000)).unwrap();
    }
    seen.extend(run_without(&mut rs, &[]));
    let retired: Vec<(usize, u64)> = seen
        .iter()
        .filter_map(|(i, a)| match a {
            Action::Retired { epoch } => Some((*i, *epoch)),
            _ => None,
        })
        .collect();
    // The joiner is not a configured member either, but epoch 0 removes nobody.
    assert_eq!(retired, [(0, 1)], "only the removed replica retires, once, from the epoch that removed it");
    let before = rs[1].commit_index();
    assert!(rs.iter().all(|r| r.commit_index() == before), "the joiner caught up");
    // Without replica 0 the new members still form a majority, on the fast path.
    let fast = rs[1].metrics().fast_path_decisions;
    rs[3].propose(T0, Bytes::from_static(b"after")).unwrap();
    run_without(&mut rs, &[0]);
    assert_eq!(rs[1].commit_index(), before + 1);
    assert_eq!(rs[3].commit_index(), before + 1);
    assert_eq!(rs[1].metrics().fast_path_decisions, fast + 1, "replica 1 now leads");
}

#[test]
fn removed_majority_retires_only_after_the_new_members_caught_up() {
    // Replicas 0 and 1 leave, 3 and 4 join: once the window has passed, only replica 2 of
    // the old members remains, so 0 and 1 must wait for the joiners to learn the old slots.
    let mut rs: Vec<Replica> = (0..5).map(|i| ordered(i, &[])).collect();
    run_without(&mut rs, &[3, 4]);
    let joiners = vec![NodeId(2), NodeId(3), NodeId(4)];
    rs[0].propose_reconfiguration(T0, joiners, vec![]).unwrap();
    let mut seen = run_without(&mut rs, &[3, 4]);
    for k in 0..70u32 {
        rs[0].propose(T0, Bytes::from(format!("w{k}"))).unwrap();
        seen.extend(run_without(&mut rs, &[3, 4]));
    }
    let from = seen
        .iter()
        .find_map(|(_, a)| match a {
            Action::Reconfigured { epoch: 1, from, .. } => Some(from.0),
            _ => None,
        })
        .expect("the change was applied");
    assert!(rs[0].commit_index() >= from, "the old members decided the whole window");
    assert_eq!(rs[3].commit_index(), 0);
    let retired = |seen: &[(usize, Action)]| {
        seen.iter()
            .filter(|(_, a)| matches!(a, Action::Retired { epoch: 1 }))
            .map(|(i, _)| *i)
            .collect::<Vec<_>>()
    };
    assert_eq!(retired(&seen), [0usize; 0], "the joiners know none of the old slots yet");
    // The joiners come back; the removed replicas' probes tell them how far the log goes,
    // they catch up, and the next probes report their durable progress.
    let mut t = 0;
    while retired(&seen).len() < 2 {
        t += 1;
        assert!(
            t < 20,
            "retirement never reported: {:?}",
            rs.iter().map(Replica::commit_index).collect::<Vec<_>>()
        );
        let now = Timestamp(t * 1_000_000_000);
        for r in &mut rs {
            r.handle_timeout(now).unwrap();
        }
        seen.extend(run_without(&mut rs, &[]));
    }
    let mut both = retired(&seen);
    both.sort_unstable();
    assert_eq!(both, [0, 1]);
    assert!(rs[3].commit_index() >= from && rs[4].commit_index() >= from, "joiners caught up first");
}

#[test]
fn half_of_an_even_new_membership_is_not_a_majority() {
    // Replica 0 leaves; the new members are 1 and 2 (old, up to date) and 3 and 4 (joining,
    // away). Two of four caught up is half, not a majority: replica 0 must keep waiting,
    // however often it asks.
    let mut rs: Vec<Replica> = (0..5).map(|i| ordered(i, &[])).collect();
    run_without(&mut rs, &[3, 4]);
    rs[0].propose_reconfiguration(T0, vec![NodeId(1), NodeId(2), NodeId(3), NodeId(4)], vec![]).unwrap();
    let mut seen = run_without(&mut rs, &[3, 4]);
    for k in 0..70u32 {
        rs[1].propose(T0, Bytes::from(format!("w{k}"))).unwrap();
        seen.extend(run_without(&mut rs, &[3, 4]));
    }
    let retired = |seen: &[(usize, Action)]| seen.iter().any(|(_, a)| matches!(a, Action::Retired { .. }));
    for t in 1..20 {
        for r in &mut rs[..3] {
            r.handle_timeout(Timestamp(t * 100_000_000)).unwrap();
        }
        seen.extend(run_without(&mut rs, &[3, 4]));
    }
    let from = seen
        .iter()
        .find_map(|(_, a)| match a {
            Action::Reconfigured { epoch: 1, from, .. } => Some(from.0),
            _ => None,
        })
        .expect("the change was applied");
    // The new epoch needs three of four, so the old members stop at its first slot.
    assert!(
        rs[1].commit_index() >= from && rs[2].commit_index() >= from,
        "the old members finished the window"
    );
    assert!(!retired(&seen), "replica 0 retired with only two of four new members caught up");
    // A third catching up is a majority.
    for t in 20..60 {
        for r in &mut rs[..4] {
            r.handle_timeout(Timestamp(t * 100_000_000)).unwrap();
        }
        seen.extend(run_without(&mut rs, &[4]));
    }
    assert!(seen.iter().any(|(i, a)| *i == 0 && matches!(a, Action::Retired { epoch: 1 })));
}

#[test]
fn a_removed_replica_added_back_while_away_does_not_retire_on_a_stale_view() {
    // Small messages, so catching up takes many pages of a few slots each.
    let small = |id: u32| {
        let mut cfg = config(&[0, 1, 2]);
        cfg.id = NodeId(id);
        (cfg.max_batch_bytes, cfg.max_message_bytes) = (64, 1152);
        Replica::new(cfg, [], T0).unwrap()
    };
    let mut rs: Vec<Replica> = (0..4).map(small).collect();
    run_without(&mut rs, &[]);
    // Replica 2 learns that it is removed, then goes away before the new members caught up.
    rs[0].propose_reconfiguration(T0, vec![NodeId(0), NodeId(1), NodeId(3)], vec![]).unwrap();
    let mut seen = run_without(&mut rs, &[]);
    let retired =
        |seen: &[(usize, Action)]| seen.iter().any(|(i, a)| *i == 2 && matches!(a, Action::Retired { .. }));
    assert!(!retired(&seen));
    // Meanwhile the window passes and a second change adds replica 2 back.
    let write = |rs: &mut Vec<Replica>, k: u32| {
        rs[0].propose(T0, Bytes::from(format!("w{k}"))).unwrap();
        run_without(rs, &[2])
    };
    for k in 0..70 {
        seen.extend(write(&mut rs, k));
    }
    rs[0].propose_reconfiguration(T0, vec![NodeId(0), NodeId(1), NodeId(2), NodeId(3)], vec![]).unwrap();
    seen.extend(run_without(&mut rs, &[2]));
    for k in 70..140 {
        seen.extend(write(&mut rs, k));
    }
    assert!(seen.iter().any(|(i, a)| *i == 0 && matches!(a, Action::Reconfigured { epoch: 2, .. })));
    // Replica 2 comes back far behind. Members report progress past the epoch it was removed
    // from, but also past what it knows: it must catch up (and find itself added back)
    // instead of retiring.
    let target = rs[0].commit_index();
    let mut t = 0;
    while rs[2].commit_index() < target {
        t += 1;
        assert!(t < 200, "replica 2 did not catch up: {} of {target}", rs[2].commit_index());
        for r in &mut rs {
            r.handle_timeout(Timestamp(t * 100_000_000)).unwrap();
        }
        seen.extend(run_without(&mut rs, &[]));
    }
    assert!(!retired(&seen), "replica 2 retired from epoch 1 although epoch 2 added it back");
}

/// Like [`run_without`], but messages to a replica in `hold` are kept in `held` instead of
/// being delivered.
fn run_holding(rs: &mut [Replica], hold: &[usize], held: &mut Vec<(usize, Input)>) -> Vec<(usize, Action)> {
    let mut queue: std::collections::VecDeque<(usize, Input)> = std::mem::take(held).into();
    held.clear();
    let mut seen = Vec::new();
    loop {
        for (i, r) in rs.iter_mut().enumerate() {
            for a in drain(r) {
                match a {
                    Action::Send(o) => queue.push_back((o.to.0 as usize, Input::Message(o.bytes))),
                    Action::Persist { id, then, .. } => {
                        queue.extend(then.into_iter().map(|o| (o.to.0 as usize, Input::Message(o.bytes))));
                        queue.push_back((i, Input::Persisted(id)));
                    }
                    other => seen.push((i, other)),
                }
            }
        }
        let Some((to, input)) = queue.pop_front() else { return seen };
        if hold.contains(&to) && !matches!(input, Input::Persisted(_)) {
            held.push((to, input));
            continue;
        }
        rs[to].handle(T0, input).unwrap();
    }
}

#[test]
fn a_replica_added_back_after_its_probes_were_answered_does_not_retire() {
    // Replica 2 leaves; it learns every old slot, and the window ends with nothing after it.
    let mut rs: Vec<Replica> = (0..4).map(|i| ordered(i, &[])).collect();
    let mut seen = run_without(&mut rs, &[]);
    rs[0].propose_reconfiguration(T0, vec![NodeId(0), NodeId(1), NodeId(3)], vec![]).unwrap();
    seen.extend(run_without(&mut rs, &[]));
    let from = seen
        .iter()
        .find_map(|(_, a)| match a {
            Action::Reconfigured { epoch: 1, from, .. } => Some(from.0),
            _ => None,
        })
        .expect("the change was applied");
    let mut k = 0;
    while rs[0].commit_index() < from {
        rs[0].propose(T0, Bytes::from(format!("w{k}"))).unwrap();
        seen.extend(run_without(&mut rs, &[]));
        k += 1;
    }
    assert_eq!(rs.iter().map(Replica::commit_index).collect::<Vec<_>>(), [from; 4]);
    // Its probes are answered, but the answers are held back...
    let mut probes = Vec::new();
    rs[2].handle_timeout(Timestamp(1_000_000_000)).unwrap();
    seen.extend(run_holding(&mut rs, &[2], &mut probes));
    assert!(!probes.is_empty(), "replica 2 asked the new members for their progress");
    // ...while a second change adds it back, before it hears of that.
    rs[0].propose_reconfiguration(T0, vec![NodeId(0), NodeId(1), NodeId(2), NodeId(3)], vec![]).unwrap();
    let mut later = Vec::new();
    seen.extend(run_holding(&mut rs, &[2], &mut later));
    assert!(rs[0].commit_index() > from, "the second change is decided");
    // The held answers say a majority caught up and nobody knows more than replica 2: only
    // its own read barrier, ordered after the second change, shows it was added back.
    seen.extend(run_holding(&mut rs, &[], &mut probes));
    seen.extend(run_holding(&mut rs, &[], &mut later));
    for t in 2..20 {
        for r in &mut rs {
            r.handle_timeout(Timestamp(t * 1_000_000_000)).unwrap();
        }
        seen.extend(run_without(&mut rs, &[]));
    }
    assert!(rs[2].commit_index() > from, "replica 2 learned the second change");
    let retired: Vec<_> = seen.iter().filter(|(_, a)| matches!(a, Action::Retired { .. })).collect();
    assert!(retired.is_empty(), "replica 2 retired although it was added back: {retired:?}");
    let ready = seen.iter().filter(|(i, a)| *i == 2 && matches!(a, Action::ReadReady { .. })).count();
    assert_eq!(ready, 0, "the retirement barrier is never reported to the application");
}

#[test]
fn a_joiner_removed_again_retires_after_its_epochs_were_pruned() {
    // Replica 3 joins (epoch 1) and learns that it leaves again (epoch 2), but is cut off
    // before the new members catch up; meanwhile a third change also without it takes
    // effect, and every peer prunes the epochs that knew it. It must still be able to catch
    // up, confirm and retire.
    let mut rs: Vec<Replica> = (0..4).map(|i| ordered(i, &[])).collect();
    let mut seen = run_without(&mut rs, &[]);
    let mut k = 0u32;
    let mut change = |rs: &mut Vec<Replica>,
                      seen: &mut Vec<(usize, Action)>,
                      members: &[u32],
                      order: &[u32],
                      cut: &[usize]| {
        let ids = |v: &[u32]| v.iter().copied().map(NodeId).collect::<Vec<_>>();
        rs[0].propose_reconfiguration(T0, ids(members), ids(order)).unwrap();
        // Everyone learns the change; `cut` is away for its window.
        seen.extend(run_without(rs, &[]));
        // Through the window.
        for _ in 0..70 {
            rs[0].propose(T0, Bytes::from(format!("w{k}"))).unwrap();
            seen.extend(run_without(rs, cut));
            k += 1;
        }
    };
    change(&mut rs, &mut seen, &[0, 1, 2, 3], &[], &[]);
    change(&mut rs, &mut seen, &[0, 1, 2], &[], &[3]);
    change(&mut rs, &mut seen, &[0, 1, 2], &[1, 0, 2], &[3]);
    assert!(seen.iter().any(|(i, a)| *i == 0 && matches!(a, Action::Reconfigured { epoch: 3, .. })));
    // Back: every request it sends comes from a replica no peer's schedule knows any more.
    for t in 1..40 {
        for r in &mut rs {
            r.handle_timeout(Timestamp(t * 1_000_000_000)).unwrap();
        }
        seen.extend(run_without(&mut rs, &[]));
    }
    assert!(
        seen.iter().any(|(i, a)| *i == 3 && matches!(a, Action::Retired { .. })),
        "replica 3 never retired (commit index {} of {})",
        rs[3].commit_index(),
        rs[0].commit_index()
    );
}

#[test]
fn a_replica_removed_while_down_learns_it_when_restarted() {
    // Replica 2 is down for its whole removal; the cluster then goes quiet. Nothing would
    // tell it what happened, so on restart it asks.
    let mut rs: Vec<Replica> = (0..4).map(|i| ordered(i, &[])).collect();
    let mut disks = vec![Vec::new(); 4];
    let run = |rs: &mut Vec<Replica>, disks: &mut Vec<Vec<Bytes>>, down: &[usize]| {
        let mut queue = std::collections::VecDeque::new();
        let mut seen = Vec::new();
        loop {
            for (i, r) in rs.iter_mut().enumerate() {
                for a in drain(r) {
                    match a {
                        Action::Send(o) => queue.push_back((o.to.0 as usize, Input::Message(o.bytes))),
                        Action::Persist { id, records, then, replace } => {
                            if replace {
                                disks[i].clear();
                            }
                            disks[i].extend(records);
                            queue
                                .extend(then.into_iter().map(|o| (o.to.0 as usize, Input::Message(o.bytes))));
                            queue.push_back((i, Input::Persisted(id)));
                        }
                        other => seen.push((i, other)),
                    }
                }
            }
            let Some((to, input)) = queue.pop_front() else { return seen };
            if down.contains(&to) {
                continue;
            }
            rs[to].handle(T0, input).unwrap();
        }
    };
    run(&mut rs, &mut disks, &[]);
    rs[0].propose(T0, Bytes::from_static(b"before")).unwrap();
    run(&mut rs, &mut disks, &[]);
    rs[0].propose_reconfiguration(T0, vec![NodeId(0), NodeId(1), NodeId(3)], vec![]).unwrap();
    run(&mut rs, &mut disks, &[2]);
    for k in 0..70u32 {
        rs[0].propose(T0, Bytes::from(format!("w{k}"))).unwrap();
        run(&mut rs, &mut disks, &[2]);
    }
    let behind = rs[2].commit_index();
    let mut cfg = config(&[0, 1, 2]);
    cfg.id = NodeId(2);
    cfg.seed = [9; 32];
    rs[2] = Replica::new(cfg, disks[2].clone(), T0).unwrap();
    // Its first requests are lost: it asks again until someone answers.
    let mut seen = run(&mut rs, &mut disks, &[0, 1, 3]);
    for t in 1..30 {
        seen.extend(run(&mut rs, &mut disks, &[]));
        for r in rs.iter_mut() {
            r.handle_timeout(Timestamp(t * 1_000_000_000)).unwrap();
        }
    }
    seen.extend(run(&mut rs, &mut disks, &[]));
    assert!(rs[2].commit_index() > behind, "replica 2 caught up after its restart");
    assert!(seen.iter().any(|(i, a)| *i == 2 && matches!(a, Action::Retired { epoch: 1 })));
}

#[test]
fn a_change_takes_effect_and_the_cluster_goes_quiet_without_writes() {
    // Nobody writes after the change: the window before it takes effect is filled anyway,
    // the removed replica retires, and then every replica is quiet.
    let mut rs: Vec<Replica> = (0..4).map(|i| ordered(i, &[])).collect();
    let mut seen = run_without(&mut rs, &[]);
    rs[0].propose_reconfiguration(T0, vec![NodeId(1), NodeId(2), NodeId(3)], vec![]).unwrap();
    seen.extend(run_without(&mut rs, &[]));
    let mut t = 1;
    while rs.iter().any(|r| r.next_deadline().is_some()) {
        assert!(t < 200, "still busy: {:?}", rs.iter().map(Replica::next_deadline).collect::<Vec<_>>());
        for r in &mut rs {
            r.handle_timeout(Timestamp(t * 100_000_000)).unwrap();
        }
        seen.extend(run_without(&mut rs, &[]));
        t += 1;
    }
    assert!(seen.iter().any(|(i, a)| *i == 3 && matches!(a, Action::Reconfigured { epoch: 1, .. })));
    assert!(seen.iter().any(|(i, a)| *i == 0 && matches!(a, Action::Retired { epoch: 1 })));
}

#[test]
fn a_change_takes_effect_without_writes_while_the_leader_is_down() {
    // The leader (0) is down when replica 1 proposes replacing it: the others fill the
    // window themselves, the change takes effect, and they go quiet.
    let mut rs: Vec<Replica> = (0..4).map(|i| ordered(i, &[])).collect();
    let mut seen = run_without(&mut rs, &[]);
    rs[1].propose_reconfiguration(T0, vec![NodeId(1), NodeId(2), NodeId(3)], vec![]).unwrap();
    seen.extend(run_without(&mut rs, &[0]));
    let mut t = 1;
    loop {
        for r in &mut rs[1..] {
            r.handle_timeout(Timestamp(t * 100_000_000)).unwrap();
        }
        seen.extend(run_without(&mut rs, &[0]));
        if rs[1..].iter().all(|r| r.next_deadline().is_none()) {
            break;
        }
        assert!(t < 400, "still busy: {:?}", rs.iter().map(Replica::commit_index).collect::<Vec<_>>());
        t += 1;
    }
    assert!(seen.iter().any(|(i, a)| *i == 3 && matches!(a, Action::Reconfigured { epoch: 1, .. })));
    let from = seen
        .iter()
        .find_map(|(_, a)| match a {
            Action::Reconfigured { epoch: 1, from, .. } => Some(from.0),
            _ => None,
        })
        .unwrap();
    assert!(rs[1..].iter().all(|r| r.commit_index() >= from), "the window was filled without the leader");
}

#[test]
fn a_joiner_in_a_cluster_that_never_changed_adds_nothing() {
    // Replica 3 runs with the configuration but is no member: with no change ever decided it
    // was removed from nothing, so it must not retire (or propose to) anything.
    let mut rs: Vec<Replica> = (0..4).map(|i| ordered(i, &[])).collect();
    let mut seen = run_without(&mut rs, &[]);
    for t in 1..20 {
        for r in &mut rs {
            r.handle_timeout(Timestamp(t * 100_000_000)).unwrap();
        }
        seen.extend(run_without(&mut rs, &[]));
    }
    assert!(rs.iter().all(|r| r.commit_index() == 0 && r.next_deadline().is_none()));
    assert!(!seen.iter().any(|(_, a)| matches!(a, Action::Retired { .. })));
}

#[test]
fn a_restarted_replica_keeps_asking_until_a_majority_answers() {
    // Replicas 0 and 1 miss what 2, 3 and 4 decide. Replica 0 restarts, and only replica 1,
    // as far behind as itself, hears it: that answer must not end its asking, or nothing in
    // a quiet cluster would ever tell it what it missed.
    let five = |id: u32, seed: u8| {
        let mut cfg = config(&[0, 1, 2, 3, 4]);
        (cfg.id, cfg.seed) = (NodeId(id), [seed; 32]);
        cfg
    };
    let mut rs: Vec<Replica> = (0..5).map(|i| Replica::new(five(i, 1), [], T0).unwrap()).collect();
    let mut disks = vec![Vec::new(); 5];
    let run = |rs: &mut Vec<Replica>, disks: &mut Vec<Vec<Bytes>>, down: &[usize]| {
        let mut queue = std::collections::VecDeque::new();
        loop {
            for (i, r) in rs.iter_mut().enumerate() {
                for a in drain(r) {
                    match a {
                        Action::Send(o) => queue.push_back((o.to.0 as usize, Input::Message(o.bytes))),
                        Action::Persist { id, records, then, replace } => {
                            if replace {
                                disks[i].clear();
                            }
                            disks[i].extend(records);
                            queue
                                .extend(then.into_iter().map(|o| (o.to.0 as usize, Input::Message(o.bytes))));
                            queue.push_back((i, Input::Persisted(id)));
                        }
                        _ => {}
                    }
                }
            }
            let Some((to, input)) = queue.pop_front() else { return };
            if down.contains(&to) && !matches!(input, Input::Persisted(_)) {
                continue;
            }
            rs[to].handle(T0, input).unwrap();
        }
    };
    run(&mut rs, &mut disks, &[]);
    for k in 0..5u32 {
        rs[2].propose(T0, Bytes::from(format!("w{k}"))).unwrap();
        run(&mut rs, &mut disks, &[0, 1]);
        for r in &mut rs[2..] {
            r.handle_timeout(Timestamp(u64::from(k + 1) * 1_000_000_000)).unwrap();
        }
        run(&mut rs, &mut disks, &[0, 1]);
    }
    let decided = rs[2].commit_index();
    assert!(decided > 0 && rs[0].commit_index() == 0 && rs[1].commit_index() == 0);
    rs[0] = Replica::new(five(0, 9), disks[0].clone(), T0).unwrap();
    run(&mut rs, &mut disks, &[2, 3, 4]);
    for t in 10..40 {
        rs[0].handle_timeout(Timestamp(t * 1_000_000_000)).unwrap();
        run(&mut rs, &mut disks, &[]);
    }
    assert_eq!(rs[0].commit_index(), decided, "replica 0 caught up after its restart");
}

#[test]
fn a_restarted_replica_counts_answers_against_the_membership_it_catches_up_into() {
    // Replica 0 is down while {0,1,2} becomes {0,3,4}, and replica 1 misses what the new
    // epoch decides next. After its restart replica 0 reaches only 1, whose page takes it
    // into the new epoch: 1 and itself are a majority of the old members but not of the
    // new ones, so it must keep asking (now 3 and 4 too).
    let mut rs: Vec<Replica> = (0..5).map(|i| ordered(i, &[])).collect();
    let mut disks = vec![Vec::new(); 5];
    let run = |rs: &mut Vec<Replica>, disks: &mut Vec<Vec<Bytes>>, down: &[usize], t: u64| {
        let now = Timestamp(t * 100_000_000);
        let (mut queue, mut timed) = (std::collections::VecDeque::new(), false);
        loop {
            for (i, r) in rs.iter_mut().enumerate() {
                for a in drain(r) {
                    if let Action::Send(o) = a {
                        queue.push_back((o.to.0 as usize, Input::Message(o.bytes)));
                    } else if let Action::Persist { id, records, then, replace } = a {
                        if replace {
                            disks[i].clear();
                        }
                        disks[i].extend(records);
                        queue.extend(then.into_iter().map(|o| (o.to.0 as usize, Input::Message(o.bytes))));
                        queue.push_back((i, Input::Persisted(id)));
                    }
                }
            }
            let Some((to, input)) = queue.pop_front() else {
                // Settled: fire the timers once, then settle again.
                if timed {
                    return;
                }
                timed = true;
                for (i, r) in rs.iter_mut().enumerate() {
                    if !down.contains(&i) {
                        r.handle_timeout(now).unwrap();
                    }
                }
                continue;
            };
            if down.contains(&to) && !matches!(input, Input::Persisted(_)) {
                continue;
            }
            rs[to].handle(now, input).unwrap();
        }
    };
    run(&mut rs, &mut disks, &[], 0);
    rs[1].propose_reconfiguration(T0, vec![NodeId(0), NodeId(3), NodeId(4)], vec![]).unwrap();
    for t in 1..200 {
        run(&mut rs, &mut disks, &[0], t);
    }
    let boundary = rs[1].commit_index();
    assert!(boundary > 0 && rs[0].commit_index() == 0);
    rs[3].propose(T0, Bytes::from_static(b"new epoch")).unwrap();
    for t in 200..300 {
        run(&mut rs, &mut disks, &[0, 1, 2], t);
    }
    let decided = rs[3].commit_index();
    assert!(decided > boundary && rs[1].commit_index() == boundary, "1 missed the new epoch");
    let mut cfg = config(&[0, 1, 2]);
    (cfg.id, cfg.seed) = (NodeId(0), [9; 32]);
    rs[0] = Replica::new(cfg, disks[0].clone(), T0).unwrap();
    run(&mut rs, &mut disks, &[2, 3, 4], 300);
    assert_eq!(rs[0].commit_index(), boundary, "replica 1 took it into the new epoch");
    for t in 301..400 {
        run(&mut rs, &mut disks, &[], t);
    }
    assert_eq!(rs[0].commit_index(), decided, "it kept asking until the new members answered");
}

#[test]
fn a_restarted_single_member_goes_quiet() {
    // Nobody to ask where the log is: no catch-up timer either.
    let mut durable = Vec::new();
    let mut r = Replica::new(config(&[0]), [], T0).unwrap();
    settle(&mut r, &mut durable);
    let mut r = Replica::new(config(&[0]), durable.clone(), T0).unwrap();
    settle(&mut r, &mut durable);
    assert_eq!(r.next_deadline(), None);
}

#[test]
fn membership_changes_are_validated() {
    let mut r = ordered(0, &[]);
    drain(&mut r);
    for (members, order) in [(vec![], vec![]), (vec![1, 1], vec![]), (vec![1, 2], vec![2])] {
        let members = members.into_iter().map(NodeId).collect();
        let order = order.into_iter().map(NodeId).collect();
        assert!(matches!(r.propose_reconfiguration(T0, members, order), Err(Error::InvalidConfig(_))));
    }
}

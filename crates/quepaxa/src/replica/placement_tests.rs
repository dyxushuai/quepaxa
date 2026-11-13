//! Leader placement measurement: round-trip probes and write counts.

use super::placement::{ACTIVE_FOR, PROBE_INTERVAL};
use super::tests::{from_peer, r_wire, replica_as};
use super::*;

const MS: u64 = 1_000_000;

fn placed(me: u32) -> Replica {
    let mut r = replica_as(me);
    r.cfg.placement = true;
    r
}

/// A probe as sent: to whom, sent when, with what table.
type Sent = (NodeId, u64, Vec<(NodeId, u64)>);

/// The `RttProbe`s among `out`.
fn probes(out: &[Action]) -> Vec<Sent> {
    out.iter()
        .filter_map(|a| match a {
            Action::Send(o) => match Message::decode(&o.bytes, &r_wire()) {
                Ok(Message { body: Body::RttProbe { sent, table, .. }, .. }) => Some((o.to, sent, table)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn rtt_replies(out: &[Action]) -> Vec<(NodeId, u64)> {
    out.iter()
        .filter_map(|a| match a {
            Action::Send(o) => match Message::decode(&o.bytes, &r_wire()) {
                Ok(Message { body: Body::RttReply { sent, .. }, .. }) => Some((o.to, sent)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Delivers `from`'s answer to a probe `r` sent it at `sent`, arriving at `at`.
fn answer(r: &mut Replica, from: u32, sent: u64, at: u64) {
    r.placement.awaiting.entry(NodeId(from)).or_default().push(sent);
    r.handle(Timestamp(at), from_peer(from, r.me.0, Body::RttReply { incarnation: r.incarnation, sent }))
        .unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
}

/// Applies one decided slot holding an application entry from each of `origins`.
fn apply_writes(r: &mut Replica, at: Timestamp, origins: &[u32]) {
    let items: Vec<Item> = origins
        .iter()
        .enumerate()
        .map(|(i, &o)| Item {
            entry: Entry {
                origin: NodeId(o),
                incarnation: 1,
                id: ProposalId(r.committed * 16 + i as u64),
                payload: Bytes::from_static(b"w"),
            },
            kind: Kind::App,
        })
        .collect();
    let decided = Proposal {
        priority: 1,
        key: ValueKey { origin: NodeId(1), incarnation: 1, seq: r.committed },
        value: batch::encode(&items),
    };
    let slot = Slot(r.committed);
    r.handle(at, from_peer(1, r.me.0, Body::Decided { slot, proposal: decided })).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
}

#[test]
fn a_write_starts_probing_every_other_member() {
    let mut r = placed(0);
    assert_eq!(r.next_deadline(), None);
    apply_writes(&mut r, Timestamp(10 * MS), &[1]);
    let at = r.next_deadline().expect("a probe is due");
    r.handle_timeout(at).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    let sent: Vec<_> = probes(&out).into_iter().map(|(to, sent, _)| (to, sent)).collect();
    assert_eq!(sent, [(NodeId(1), at.0), (NodeId(2), at.0)]);
    assert_eq!(r.placement.next_probe, Some(at.after(PROBE_INTERVAL)));
}

#[test]
fn round_trips_converge_and_an_outlier_moves_the_average_by_an_eighth() {
    let mut r = placed(0);
    let mut t = 0;
    for _ in 0..4 {
        t += PROBE_INTERVAL;
        answer(&mut r, 1, t, t + 80 * MS);
    }
    assert_eq!(r.fresh_rtt(NodeId(1)), Some(80 * MS));
    t += PROBE_INTERVAL;
    answer(&mut r, 1, t, t + 880 * MS);
    assert_eq!(r.fresh_rtt(NodeId(1)), Some(80 * MS - 10 * MS + 110 * MS));
    // A reply claiming a send time in the future is ignored.
    answer(&mut r, 1, t + 900 * MS, t + 881 * MS);
    assert_eq!(r.fresh_rtt(NodeId(1)), Some(180 * MS));
}

#[test]
fn a_round_trip_not_sampled_for_a_while_is_unknown() {
    let mut r = placed(0);
    let mut out = Vec::new();
    answer(&mut r, 1, MS, 5 * MS);
    assert_eq!(r.fresh_rtt(NodeId(1)), Some(4 * MS));
    let later = 5 * MS + 5 * PROBE_INTERVAL;
    r.handle_timeout(Timestamp(later)).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.fresh_rtt(NodeId(1)), None);
    // The next sample starts a new average rather than blending into the stale one.
    answer(&mut r, 1, later, later + 200 * MS);
    assert_eq!(r.fresh_rtt(NodeId(1)), Some(200 * MS));
}

#[test]
fn only_the_reply_to_the_latest_probe_counts() {
    let mut r = placed(0);
    let mut out = Vec::new();
    // Unsolicited: no probe went to member 1.
    r.handle(Timestamp(5 * MS), from_peer(1, 0, Body::RttReply { incarnation: r.incarnation, sent: MS }))
        .unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.fresh_rtt(NodeId(1)), None);
    answer(&mut r, 1, 10 * MS, 12 * MS);
    assert_eq!(r.fresh_rtt(NodeId(1)), Some(2 * MS));
    // A duplicate of the same reply, arriving much later, is not sampled again.
    r.handle(
        Timestamp(900 * MS),
        from_peer(1, 0, Body::RttReply { incarnation: r.incarnation, sent: 10 * MS }),
    )
    .unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.fresh_rtt(NodeId(1)), Some(2 * MS));
    // Nor is a reply to a probe older than one already answered.
    r.placement.awaiting.insert(NodeId(1), alloc::vec![920 * MS, 930 * MS]);
    answer(&mut r, 1, 940 * MS, 941 * MS);
    r.handle(
        Timestamp(950 * MS),
        from_peer(1, 0, Body::RttReply { incarnation: r.incarnation, sent: 920 * MS }),
    )
    .unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.placement.awaiting[&NodeId(1)], [] as [u64; 0]);
    assert_eq!(r.fresh_rtt(NodeId(1)), Some(2 * MS - MS / 4 + MS / 8));
}

#[test]
fn a_round_trip_longer_than_the_probe_interval_is_measured() {
    let mut r = placed(0);
    let mut out = Vec::new();
    apply_writes(&mut r, Timestamp(MS), &[1]);
    // Probes go out every second; member 1 answers the first one 1.5 s later.
    let first = r.next_deadline().unwrap();
    r.handle_timeout(first).unwrap();
    r.poll_actions(&mut out);
    r.handle_timeout(first.after(PROBE_INTERVAL)).unwrap();
    r.poll_actions(&mut out);
    let late = first.after(PROBE_INTERVAL + PROBE_INTERVAL / 2);
    r.handle(late, from_peer(1, 0, Body::RttReply { incarnation: r.incarnation, sent: first.0 })).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.fresh_rtt(NodeId(1)), Some(PROBE_INTERVAL + PROBE_INTERVAL / 2));
}

#[test]
fn a_probe_is_answered_and_its_table_kept() {
    let mut r = placed(0);
    let mut out = Vec::new();
    let table = alloc::vec![(NodeId(0), 70 * MS), (NodeId(2), 30 * MS), (NodeId(9), MS)];
    r.handle(
        Timestamp(3 * MS),
        from_peer(1, 0, Body::RttProbe { incarnation: 7, sent: 12345, commit: 0, table }),
    )
    .unwrap();
    r.poll_actions(&mut out);
    assert_eq!(rtt_replies(&out), [(NodeId(1), 12345)]);
    let kept = &r.placement.tables[&NodeId(1)];
    assert_eq!(kept.0, [(NodeId(0), 70 * MS), (NodeId(2), 30 * MS)], "only members in force");
    assert_eq!(kept.1, Timestamp(3 * MS));
}

#[test]
fn the_probe_carries_this_members_fresh_round_trips() {
    let mut r = placed(0);
    let mut out = Vec::new();
    answer(&mut r, 2, MS, 5 * MS);
    apply_writes(&mut r, Timestamp(6 * MS), &[0]);
    let at = r.next_deadline().unwrap();
    r.handle_timeout(at).unwrap();
    r.poll_actions(&mut out);
    let tables: Vec<_> = probes(&out).into_iter().map(|(_, _, t)| t).collect();
    assert_eq!(tables, [alloc::vec![(NodeId(2), 4 * MS)], alloc::vec![(NodeId(2), 4 * MS)]]);
}

#[test]
fn an_idle_cluster_stops_probing_and_goes_quiet() {
    let mut r = placed(0);
    let mut out = Vec::new();
    apply_writes(&mut r, Timestamp(MS), &[2]);
    let mut probed = 0;
    while let Some(at) = r.next_deadline() {
        assert!(at.0 <= MS + ACTIVE_FOR + 2 * PROBE_INTERVAL, "still probing at {at:?}");
        r.handle_timeout(at).unwrap();
        r.poll_actions(&mut out);
        probed += probes(&out).len();
        out.clear();
    }
    assert!(probed > 0);
    // A new write starts it again.
    apply_writes(&mut r, Timestamp(MS + 60 * PROBE_INTERVAL), &[2]);
    assert!(r.next_deadline().is_some());
}

#[test]
fn a_member_without_placement_neither_probes_nor_answers() {
    let mut r = replica_as(0);
    let mut out = Vec::new();
    apply_writes(&mut r, Timestamp(MS), &[1]);
    assert_eq!(r.next_deadline(), None);
    assert!(r.placement.writes.is_empty(), "nothing is counted");
    r.handle(
        Timestamp(2 * MS),
        from_peer(1, 0, Body::RttProbe { incarnation: 7, sent: 7, commit: 0, table: Vec::new() }),
    )
    .unwrap();
    r.poll_actions(&mut out);
    assert!(rtt_replies(&out).is_empty());
    assert!(r.placement.tables.is_empty());
    answer(&mut r, 1, MS, 3 * MS);
    assert_eq!(r.fresh_rtt(NodeId(1)), None);
}

#[test]
fn writes_are_counted_by_origin_and_reads_are_not() {
    let mut r = placed(0);
    apply_writes(&mut r, Timestamp(MS), &[1, 1, 2]);
    apply_writes(&mut r, Timestamp(2 * MS), &[2, 0]);
    let mut out = Vec::new();
    r.read_barrier(Timestamp(3 * MS)).unwrap();
    r.poll_actions(&mut out);
    let counts: Vec<_> = r.placement.writes.iter().map(|(n, c)| (n.0, *c)).collect();
    assert_eq!(counts, [(0, 1), (1, 2), (2, 2)]);
}

#[test]
fn a_restarted_replica_measures_afresh() {
    let mut cfg = Config::new([3; 16], NodeId(0), alloc::vec![NodeId(0), NodeId(1), NodeId(2)], [9; 32]);
    cfg.placement = true;
    let r = Replica::new(cfg, [], Timestamp(0)).unwrap();
    assert!(r.placement.rtt.is_empty() && r.placement.tables.is_empty() && r.placement.writes.is_empty());
    assert_eq!(r.placement.next_probe, None);
}

#[test]
fn probing_lasts_four_rounds_past_the_last_write_and_remembers_four_probes() {
    let mut r = placed(0);
    let mut out = Vec::new();
    apply_writes(&mut r, Timestamp(MS), &[1]);
    let mut rounds = Vec::new();
    while let Some(at) = r.next_deadline() {
        r.handle_timeout(at).unwrap();
        r.poll_actions(&mut out);
        rounds.extend(probes(&out).first().map(|(_, sent, _)| *sent));
        out.clear();
    }
    // Spelled out rather than from the constants, so a change to them shows.
    let expected: Vec<u64> = (0..5).map(|i| MS + i * PROBE_INTERVAL).collect();
    assert_eq!(rounds, expected, "the last write at 1 ms, then four seconds active");
    assert_eq!(r.placement.awaiting[&NodeId(1)], expected[1..], "unanswered: the four latest");
}

#[test]
fn a_reply_within_the_tick_of_its_probe_measures_zero() {
    let mut r = placed(0);
    answer(&mut r, 1, 5 * MS, 5 * MS);
    assert_eq!(r.fresh_rtt(NodeId(1)), Some(0));
}

#[test]
fn a_reply_to_another_incarnations_probe_is_ignored() {
    let mut r = placed(0);
    let mut out = Vec::new();
    // Same send time, but echoed for a previous incarnation (its clock started over).
    r.placement.awaiting.entry(NodeId(1)).or_default().push(MS);
    let old = r.incarnation - 1;
    r.handle(Timestamp(5 * MS), from_peer(1, 0, Body::RttReply { incarnation: old, sent: MS })).unwrap();
    r.poll_actions(&mut out);
    assert_eq!(r.fresh_rtt(NodeId(1)), None);
    assert_eq!(r.placement.awaiting[&NodeId(1)], [MS], "the probe still awaits its own reply");
}

/// One probe round at `t` after `latencies` own commits (ns); the tail it reports.
fn round_with(r: &mut Replica, t: u64, latencies: &[u64]) -> Option<u64> {
    for &ns in latencies {
        r.note_commit_latency(ns);
    }
    r.now = Timestamp(t);
    r.placement.last_write = Some(r.now);
    r.placement.next_probe = Some(r.now);
    r.fire_probe();
    r.placement.tail
}

#[test]
fn a_round_without_own_commits_reports_no_tail() {
    let mut r = placed(0);
    assert_eq!(round_with(&mut r, PROBE_INTERVAL, &[2 * MS; 10]), Some(2 * MS));
    assert_eq!(round_with(&mut r, 2 * PROBE_INTERVAL, &[]), None, "an older tail is not fresh");
}

#[test]
fn the_tail_spans_the_whole_round_however_many_commits() {
    // 1,024 fast commits, then 2,000 slow ones: most of the round was slow.
    let mut r = placed(0);
    let mut l = alloc::vec![MS; 1024];
    l.extend(core::iter::repeat_n(10 * MS, 2000));
    assert_eq!(round_with(&mut r, PROBE_INTERVAL, &l), Some(10 * MS));
    assert!(
        r.placement.latencies.is_empty() && r.placement.latency_stride == 1,
        "the next round starts over"
    );
}

#[test]
fn a_tenure_starts_when_the_replica_starts_not_at_the_clocks_epoch() {
    let mut cfg = Config::new([3; 16], NodeId(0), alloc::vec![NodeId(0), NodeId(1), NodeId(2)], [9; 32]);
    cfg.placement = true;
    let start = Timestamp(1_000 * PROBE_INTERVAL);
    let r = Replica::new(cfg, [], start).unwrap();
    assert_eq!(r.placement.policy.tenure().since(), start);
}

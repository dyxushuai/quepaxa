//! The disk's health, one transition at a time.

use super::disk::{Disk, Health};
use super::tests::{from_peer, proposal};
use super::watch_tests::leader_with_a_stuck_disk;
use super::*;

const MS: u64 = 1_000_000;
/// The hedge delay δ in these tests.
const D: u64 = 80 * MS;

fn t(ms: u64) -> Timestamp {
    Timestamp(ms * MS)
}

/// A disk whose writes usually take 1 ms, with one write issued at 1 ms.
fn writing() -> Disk {
    let mut disk = Disk::default();
    disk.assume_usual(MS);
    disk.issued(1, t(1));
    disk
}

#[test]
fn a_write_issued_on_an_idle_disk_is_in_flight() {
    let mut disk = Disk::default();
    disk.assume_usual(MS);
    assert_eq!(disk.health(t(0), D), Health::Idle);
    disk.issued(1, t(1));
    assert_eq!(disk.health(t(1), D), Health::Writing);
    assert_eq!(disk.in_flight().collect::<Vec<_>>(), [1]);
}

#[test]
fn a_disk_with_no_usual_write_time_is_never_judged_stuck() {
    let mut disk = Disk::default();
    disk.issued(1, t(1));
    assert_eq!(disk.health(Timestamp(u64::MAX), D), Health::Writing);
    assert_eq!(disk.alone_at(D), None);
}

#[test]
fn time_alone_moves_a_writing_disk_into_a_shared_stall_after_delta() {
    let disk = writing();
    assert_eq!(disk.health(Timestamp(81 * MS - 1), D), Health::Writing);
    assert_eq!(disk.health(t(81), D), Health::SharedStall { since: t(81) });
    assert_eq!(disk.health(t(500), D), Health::SharedStall { since: t(81) }, "time alone never ends it");
}

#[test]
fn stuck_waits_at_least_four_usual_write_times() {
    let mut disk = writing();
    disk.assume_usual(30 * MS);
    assert_eq!(disk.health(t(120), D), Health::Writing);
    assert_eq!(disk.health(t(121), D), Health::SharedStall { since: t(121) });
}

#[test]
fn a_decision_after_the_oldest_write_was_due_means_the_disk_is_stuck_alone() {
    let mut disk = writing();
    // Due at 5 ms (four usual write times after 1 ms): a decision by then may have been made
    // durable before every disk stopped.
    disk.decided_elsewhere(t(5));
    assert_eq!(disk.health(t(81), D), Health::SharedStall { since: t(81) });
    disk.decided_elsewhere(t(5).after(1));
    assert_eq!(disk.health(t(81), D), Health::Stuck { since: t(81) });
}

#[test]
fn a_decision_ends_a_shared_stall_already_under_way() {
    let mut disk = writing();
    assert_eq!(disk.health(t(90), D), Health::SharedStall { since: t(81) });
    disk.decided_elsewhere(t(90));
    assert_eq!(disk.health(t(90), D), Health::Stuck { since: t(81) });
}

#[test]
fn completing_the_writes_in_a_shared_stall_records_the_resumption() {
    let mut disk = writing();
    disk.issued(2, t(2));
    disk.completed(2, t(150), D);
    assert_eq!(disk.resumed(), t(150));
    assert_eq!(disk.health(t(150), D), Health::Idle);
    let first = MS - MS / 8 + 149 * MS / 8;
    assert_eq!(disk.usual(), Some(first - first / 8 + 148 * MS / 8), "one sample per write");
}

#[test]
fn completing_writes_while_stuck_alone_or_writing_is_no_resumption() {
    let mut disk = writing();
    disk.decided_elsewhere(t(10));
    disk.completed(1, t(150), D);
    assert_eq!(disk.resumed(), t(0), "others decided: only this disk was stuck");
    disk.issued(2, t(151));
    disk.completed(2, t(152), D);
    assert_eq!(disk.resumed(), t(0), "writing, not stuck");
}

#[test]
fn a_completion_leaves_later_writes_in_flight() {
    let mut disk = writing();
    disk.issued(2, t(2));
    disk.completed(1, t(3), D);
    assert_eq!(disk.in_flight().collect::<Vec<_>>(), [2]);
    assert_eq!(disk.health(t(3), D), Health::Writing);
}

#[test]
fn a_disk_that_keeps_completing_writes_counts_stuck_from_its_last_completion() {
    // The #185 hold on a slow disk: the second write waits δ behind the first, while the disk
    // keeps completing writes. Counted from the oldest write's issue, it seemed stuck at 82 ms.
    let mut disk = writing();
    disk.issued(2, t(2));
    disk.completed(1, t(40), D);
    assert_eq!(disk.health(t(119), D), Health::Writing);
    assert_eq!(disk.health(t(120), D), Health::SharedStall { since: t(120) });
}

#[test]
fn stepping_aside_waits_for_a_decision_after_the_oldest_write() {
    let mut disk = writing();
    assert_eq!(disk.alone_at(4 * D), None, "nobody decided: maybe one shared disk");
    disk.decided_elsewhere(t(1));
    assert_eq!(disk.alone_at(4 * D), None, "decided as the write was issued: not since");
    disk.decided_elsewhere(t(1).after(1));
    assert_eq!(disk.alone_at(4 * D), Some(t(321)));
    disk.assume_usual(100 * MS);
    assert_eq!(disk.alone_at(4 * D), Some(t(401)), "at least four usual write times");
    disk.completed(1, t(402), D);
    assert_eq!(disk.alone_at(4 * D), None, "caught up");
}

#[test]
fn stepping_aside_and_back_alternate_once_per_stall() {
    let (mut r, _) = leader_with_a_stuck_disk();
    let mut out = Vec::new();
    let decided = Body::Decided { slot: Slot(9), proposal: proposal(1, 1, 0) };
    r.handle(t(10), from_peer(1, 0, decided)).unwrap();
    r.poll_actions(&mut out);
    for at in [81, 90, 100] {
        r.handle_timeout(t(at)).unwrap();
        r.poll_actions(&mut out);
        assert!(r.watch.aside);
        assert_eq!(r.metrics.stepped_aside, 1, "one step aside at {at} ms");
    }
    let last = r.watch.disk.in_flight().next_back().expect("writes in flight");
    r.handle(t(101), Input::Persisted(PersistId(last))).unwrap();
    r.poll_actions(&mut out);
    assert!(!r.watch.aside, "back once the disk catches up");
    assert_eq!(r.metrics.stepped_aside, 1);
}

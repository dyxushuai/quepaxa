//! Replaces a member of a real 3-process cluster: a fourth replica joins, the first leaves
//! and exits by itself once the new members no longer need it.

mod common;

use std::time::Duration;

use common::Cluster;

#[test]
fn a_member_is_replaced_and_the_removed_replica_exits_by_itself() {
    // Replicas 0-2 are configured; replica 3 starts as a joiner.
    let mut c = Cluster::with_members(4, 3);
    for k in 0..10 {
        assert_eq!(c.request(1, &format!("PUT k{k} v{k}")), "OK");
    }
    // A replica without an address would be decided all the same and never reached.
    assert!(c.request_once(1, "RECONFIGURE 1,2,9").starts_with("ERR unknown replica 9"));
    // Replace 0 with 3, which becomes the preferred leader.
    assert_eq!(c.request(1, "RECONFIGURE 1,2,3 LEADERS 3,1,2"), "OK");
    // The change takes effect 64 slots after it is decided.
    for k in 10..80 {
        assert_eq!(c.request(1 + k % 2, &format!("PUT k{k} v{k}")), "OK");
    }
    assert!(c.wait_exit(0, Duration::from_secs(30)), "replica 0 exited with an error");
    // The new members carry on without it, the joiner included.
    assert_eq!(c.request(3, "PUT after yes"), "OK");
    assert_eq!(c.request(2, "GET after"), "VALUE yes");
    assert_eq!(c.request(3, "GET k5"), "VALUE v5");
}

#[test]
fn a_non_voting_observer_follows_and_reads_across_reconfigurations() {
    // Replicas 0-2 are voting members; replica 3 runs as a non-voting observer.
    let c = Cluster::with_members(4, 3);
    for k in 0..10 {
        assert_eq!(c.request(0, &format!("PUT o{k} val{k}")), "OK");
    }
    // Observer 3 serves linearizable reads without voting.
    assert_eq!(c.request(3, "GET o9"), "VALUE val9");
    assert_eq!(c.request(3, "GET o0"), "VALUE val0");

    // Reorder the voting leaders: observer 3 was never a voting member, so it must stay
    // running (never exit on Retired) and continue following and serving reads.
    assert_eq!(c.request(0, "LEADERS 2,1,0"), "OK");
    for k in 10..85 {
        assert_eq!(c.request(k % 3, &format!("PUT o{k} val{k}")), "OK");
    }
    // Background follow_interval must catch observer 3 up without needing a linearizable GET first.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while c.request(3, "STALE o84") != "VALUE val84" {
        assert!(std::time::Instant::now() < deadline, "observer 3 did not follow o84 via background polling");
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(c.request(3, "GET o84"), "VALUE val84");
}

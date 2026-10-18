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

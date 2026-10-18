//! Runs a real 3-process cluster: writes, linearizable reads, `kill -9` of a minority,
//! and restart of the killed replica from its data directory.

mod common;

use common::Cluster;

#[test]
fn survives_kill_9_of_a_minority_and_recovers_the_victim() {
    let mut c = Cluster::new(3);
    for k in 0..60 {
        assert_eq!(c.request(k % 3, &format!("PUT k{k} v{k}")), "OK");
    }
    assert_eq!(c.request(2, "GET k7"), "VALUE v7");

    // The initial leader (node 0) dies hard; the survivors keep committing.
    c.kill(0);
    for k in 60..120 {
        assert_eq!(c.request(1 + k % 2, &format!("PUT k{k} v{k}")), "OK");
    }
    assert_eq!(c.request(1, "GET k100"), "VALUE v100");

    // The victim restarts from disk and catches up on what it missed.
    c.start(0);
    assert_eq!(c.request(0, "GET k119"), "VALUE v119");
    assert_eq!(c.request(0, "GET k3"), "VALUE v3");
}

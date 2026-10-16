//! Three replicas in one process: writes from any replica, a linearizable read, and what
//! the replicas report.
//!
//! ```text
//! cargo run -p quepaxa --example cluster
//! ```

mod common;

use common::{Cluster, show};

fn main() {
    let mut cluster = Cluster::new(3);

    // Any replica accepts writes. Replica 0 leads (the lowest id by default), so its own
    // proposals take the one-round-trip fast path; the others forward theirs to it.
    let a = cluster.propose(0, "x = 1");
    let b = cluster.propose(2, "y = 2");
    cluster.settle();

    // Every replica applied the same log, in the same order.
    for (i, node) in cluster.nodes.iter().enumerate() {
        println!("replica {i} applied {:?}", show(&node.applied));
        assert_eq!(node.applied, cluster.nodes[0].applied);
    }
    // Each proposal is reported `Committed` exactly once, at the replica that proposed it.
    assert_eq!(cluster.nodes[0].committed, [a]);
    assert_eq!(cluster.nodes[2].committed, [b]);

    // A read barrier gives a linearizable read point: once the application has applied
    // every slot up to the reported one, reading its state sees every write that finished
    // before the barrier was requested.
    let read = cluster.read_barrier(1);
    cluster.settle();
    let (id, slot) = cluster.nodes[1].ready[0];
    assert_eq!(id, read);
    assert!(cluster.nodes[1].next_slot > slot.0, "applied through the barrier");
    println!("replica 1 may serve reads of state through slot {}", slot.0);

    // Counters for observability.
    let m = cluster.replica(0).metrics().clone();
    println!(
        "replica 0: {} slots decided, {} on the fast path, {} retransmits",
        m.slots_decided, m.fast_path_decisions, m.retransmits
    );
}

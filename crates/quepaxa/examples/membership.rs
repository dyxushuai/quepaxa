//! Replacing a member: replica 3 joins, replica 0 leaves, and replica 3 becomes the
//! preferred leader.
//!
//! ```text
//! cargo run -p quepaxa --example membership
//! ```

mod common;

use common::{Cluster, config};
use quepaxa::NodeId;

fn main() {
    // Replica 3 starts with the cluster's original configuration: its own id need not be a
    // member. It takes no part until a membership change includes it.
    let original = [0, 1, 2];
    let mut cluster = Cluster::from_configs(vec![
        config(0, &original, 0),
        config(1, &original, 0),
        config(2, &original, 0),
        config(3, &original, 0),
    ]);
    cluster.propose(1, "before");
    cluster.settle();

    // The change is an entry in the log. Decided in slot s, it governs slots s + 64 on, so
    // every replica switches at the same slot without any extra agreement.
    cluster.propose_reconfiguration(1, &[1, 2, 3], &[3, 1, 2]);
    cluster.settle();

    // Fill the 64-slot window with ordinary writes.
    for k in 0..70 {
        cluster.propose(1, &format!("w{k}"));
        cluster.settle();
    }
    for (i, node) in cluster.nodes.iter().enumerate() {
        println!("replica {i}: {:?}", node.reconfigured);
    }
    let (epoch, from, ref members) = cluster.nodes[3].reconfigured[0];
    assert_eq!((epoch, members.as_slice()), (1, &[NodeId(3), NodeId(1), NodeId(2)][..]));
    println!(
        "epoch {epoch} starts at slot {}; replica 0 may shut down once it applied slot {}",
        from.0,
        from.0 - 1
    );

    // Replica 0 leaves; the new members carry on, replica 3 leading.
    cluster.crash(0);
    cluster.propose(2, "after");
    cluster.settle();
    assert_eq!(cluster.nodes[3].applied.last().map(|b| &b[..]), Some(&b"after"[..]));
    println!("replica 3 decided {} slots on the fast path", cluster.replica(3).metrics().fast_path_decisions);
}

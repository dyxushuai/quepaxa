//! Crash and restart: recovery from durable records, compaction with an application
//! snapshot, and a lagging replica catching up from a peer's snapshot.
//!
//! ```text
//! cargo run -p quepaxa --example restart
//! ```

mod common;

use common::{Cluster, show};

fn main() {
    let mut cluster = Cluster::new(3);
    for k in 0..5 {
        cluster.propose(0, &format!("w{k}"));
    }
    cluster.settle();

    // A crash loses memory, not the disk. Restarting from the durable records (in write
    // order) with a fresh seed recovers the log; the replica emits the recovered slots
    // again, and the application skips the ones it already has.
    cluster.crash(1);
    cluster.restart(1, 1);
    cluster.settle();
    println!("replica 1 after restart: {:?}", show(&cluster.nodes[1].applied));
    assert_eq!(cluster.nodes[1].applied, cluster.nodes[0].applied);

    // Compaction: the application snapshots its state together with the replica's
    // `core_state`, then the replica drops the log the snapshot covers and rewrites its
    // storage (a `Persist` with `replace: true`).
    let before = cluster.nodes[0].disk.len();
    cluster.snapshot_and_compact(0);
    println!("replica 0 storage: {before} records before compaction, {} after", cluster.nodes[0].disk.len());

    // Replica 2 misses everything that follows. When it comes back, its peers no longer
    // have the slots it needs; it reports `SnapshotNeeded`, installs replica 0's snapshot
    // and fetches the rest of the log.
    cluster.crash(2);
    for k in 5..10 {
        cluster.propose(0, &format!("w{k}"));
    }
    cluster.settle();
    cluster.snapshot_and_compact(0);
    cluster.snapshot_and_compact(1);
    cluster.propose(0, "w10");
    cluster.settle();
    cluster.restart(2, 1);
    cluster.settle();
    // In a quiet cluster nothing tells replica 2 it is behind: it catches up on demand.
    // A linearizable read is such a demand: the barrier reaches the leader, the replica
    // learns how far the log goes, installs a snapshot and fetches the rest, and the read
    // is ready only once it has applied everything before the barrier.
    cluster.read_barrier(2);
    cluster.run_for(1_000_000_000);
    assert_eq!(cluster.nodes[2].ready.len(), 1);
    println!("replica 2 caught up: {:?}", show(&cluster.nodes[2].applied));
    assert_eq!(cluster.nodes[2].applied, cluster.nodes[0].applied);
}

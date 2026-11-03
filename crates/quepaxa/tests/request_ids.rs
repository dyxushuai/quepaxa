//! Request identities and bounded deduplication across reads and compaction.

mod common;

use bytes::Bytes;
use common::{T0, config, settle};
use quepaxa::{Action, Replica, Slot};

#[test]
fn reads_do_not_make_compacted_dedup_state_grow_with_completed_writes() {
    for read_every in [1, 1000] {
        let mut r = Replica::new(config(&[0]), [], T0).unwrap();
        let mut disk = Vec::new();
        settle(&mut r, &mut disk);
        r.propose(T0, Bytes::new()).unwrap();
        settle(&mut r, &mut disk);
        let baseline = r.core_state().len();
        let mut ids = std::collections::BTreeSet::new();
        for n in 0..1000 {
            if n % read_every == 0 {
                let read = r.read_barrier(T0).unwrap();
                assert!(ids.insert(read), "read id must be unique");
                assert!(
                    settle(&mut r, &mut disk)
                        .iter()
                        .any(|a| matches!(a, Action::ReadReady { id, .. } if *id == read))
                );
            }
            let write = r.propose(T0, Bytes::new()).unwrap();
            assert!(ids.insert(write), "write id must not collide with a read");
            assert!(
                settle(&mut r, &mut disk)
                    .iter()
                    .any(|a| matches!(a, Action::Committed { id, .. } if *id == write))
            );
            if n % 100 == 99 {
                r.compact(T0, Slot(r.emitted_index() - 1), r.core_state()).unwrap();
                settle(&mut r, &mut disk);
                assert_eq!(r.core_state().len(), baseline, "read_every={read_every}, writes={}", n + 1);
            }
        }
    }
}

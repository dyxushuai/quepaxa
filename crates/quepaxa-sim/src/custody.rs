//! G6 content custody invariant over replicas' durable disks.
//!
//! - **G6a**: replaying each replica's simulated disk in order, every `Recorder` record's
//!   references (`ENTRY_REF` items of `first`, `agg`, `prev_agg`) have a matching `Data`
//!   record earlier on that disk.
//! - **G6b**: every decided slot has its referenced content in durable `Data` records, or
//!   is below that replica's durable floor (`Compacted`), at a majority of the slot's
//!   members ([`Epochs::members`]).

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use quepaxa::inspect::{self, DataIdView, DiskCustody};

use crate::invariants::Epochs;

/// Per-execution G6 custody checker across all replicas' durable disks.
#[derive(Clone, Debug, Default)]
pub struct Custody {
    disks: Vec<DiskCustody>,
    /// Referenced data ids of every decided slot known to have at least one reference.
    decided_refs: BTreeMap<u64, Vec<DataIdView>>,
    /// Number of decided slots already scanned from `History::decided`.
    known_decided: usize,
    /// Number of applied epochs already accounted for.
    known_epochs: usize,
    /// Whether any disk changed since the last G6b check.
    dirty: bool,
}

impl Custody {
    /// Fresh custody state for `nodes` empty disks.
    pub fn new(nodes: usize) -> Self {
        Self { disks: vec![DiskCustody::default(); nodes], ..Self::default() }
    }

    /// Checks newly appended records on `node`'s disk against G6a.
    pub fn on_append(&mut self, node: usize, added: &[Bytes]) -> Result<(), String> {
        if added.is_empty() {
            return Ok(());
        }
        self.disks[node].apply(added).map_err(|e| format!("G6a content custody: n{node} {e}"))?;
        self.dirty = true;
        Ok(())
    }

    /// Replays `node`'s full disk after a `replace: true` checkpoint, checking G6a.
    pub fn on_replace(&mut self, node: usize, records: &[Bytes]) -> Result<(), String> {
        self.disks[node] =
            inspect::custody(records).map_err(|e| format!("G6a content custody: n{node} {e}"))?;
        self.dirty = true;
        Ok(())
    }

    /// Checks G6b for every decided slot across all replicas' durable disks.
    pub fn check_decided(
        &mut self,
        epochs: &Epochs,
        decided: &BTreeMap<u64, (u32, u64, u64, u64)>,
    ) -> Result<(), String> {
        if !self.dirty && decided.len() == self.known_decided && epochs.applied.len() == self.known_epochs {
            return Ok(());
        }
        for disk in &self.disks {
            for (&slot, refs) in &disk.decided {
                if !refs.is_empty() {
                    self.decided_refs.entry(slot).or_insert_with(|| refs.clone());
                }
            }
        }
        for (&slot, val) in decided {
            if self.decided_refs.contains_key(&slot) {
                continue;
            }
            if let Some(refs) = self.disks.iter().find_map(|d| d.value_refs.get(val))
                && !refs.is_empty()
            {
                self.decided_refs.insert(slot, refs.clone());
            }
        }
        for (&slot, refs) in &self.decided_refs {
            let members = epochs.members(slot);
            let covered: BTreeSet<usize> = members
                .iter()
                .copied()
                .filter(|&m| {
                    self.disks
                        .get(m)
                        .is_some_and(|d| d.floor > slot || refs.iter().all(|id| d.data.contains(id)))
                })
                .collect();
            if !epochs.majority(slot, &covered) {
                return Err(format!(
                    "G6b content custody: slot {slot} refs {refs:?} covered durably only at {covered:?} of members {members:?}"
                ));
            }
        }
        self.dirty = false;
        self.known_decided = decided.len();
        self.known_epochs = epochs.applied.len();
        Ok(())
    }
}

/// Replays every replica's disk from scratch and checks both G6a and G6b.
pub fn check_disks(
    epochs: &Epochs,
    disks: &[&[Bytes]],
    decided: &BTreeMap<u64, (u32, u64, u64, u64)>,
) -> Result<(), String> {
    let mut c = Custody::new(disks.len());
    for (i, disk) in disks.iter().enumerate() {
        c.on_replace(i, disk)?;
    }
    c.check_decided(epochs, decided)
}

#[cfg(test)]
mod tests {
    use super::*;
    use quepaxa::inspect::{
        compacted_record, data_record, decided_record, incarnation_record, proposal_value, recorder_record,
        ref_batch,
    };

    fn id(origin: u32, seq: u64) -> DataIdView {
        DataIdView { origin, incarnation: 1, seq }
    }

    fn epochs(members: Vec<usize>) -> Epochs {
        Epochs { initial: members, applied: BTreeMap::new() }
    }

    #[test]
    fn g6a_accepts_recorder_when_data_precedes_it() {
        let d1 = id(1, 0);
        let d2 = id(2, 0);
        let b1 = b"batch-one";
        let b2 = b"batch-two";
        let v1 = ref_batch(&[(d1, b1)]);
        let v2 = ref_batch(&[(d1, b1), (d2, b2)]);
        let disk = vec![
            incarnation_record(1),
            data_record(d1, Bytes::from_static(b1)),
            data_record(d2, Bytes::from_static(b2)),
            recorder_record(0, Some(v1.clone()), Some(v2), Some(v1)),
        ];
        let mut c = Custody::new(1);
        assert!(c.on_replace(0, &disk).is_ok());

        // Incremental append yields the identical DiskCustody.
        let mut inc = Custody::new(1);
        for rec in &disk {
            inc.on_append(0, std::slice::from_ref(rec)).unwrap();
        }
        assert_eq!(inc.disks[0], c.disks[0]);
    }

    #[test]
    fn g6a_rejects_recorder_with_missing_data() {
        let d1 = id(1, 0);
        let d2 = id(2, 0);
        let v1 = ref_batch(&[(d1, b"hello")]);
        let v2 = ref_batch(&[(d2, b"world")]);

        // Missing in `first` at index 0.
        let err = inspect::custody(&[recorder_record(0, Some(v1.clone()), None, None)]).unwrap_err();
        assert!(err.contains("record 0"), "{err}");
        let mut c = Custody::new(1);
        let err = c.on_replace(0, &[recorder_record(0, Some(v1.clone()), None, None)]).unwrap_err();
        assert!(err.starts_with("G6a content custody"), "{err}");
        assert!(err.contains("record 0"), "{err}");

        // Missing in `agg` at index 1.
        let disk = vec![incarnation_record(1), recorder_record(0, None, Some(v1.clone()), None)];
        let err = c.on_replace(0, &disk).unwrap_err();
        assert!(err.starts_with("G6a content custody") && err.contains("record 1"), "{err}");

        // Missing in `prev_agg` at index 2 even when `first` is present.
        let disk = vec![
            incarnation_record(1),
            data_record(d1, Bytes::from_static(b"hello")),
            recorder_record(0, Some(v1), None, Some(v2)),
        ];
        let err = c.on_replace(0, &disk).unwrap_err();
        assert!(err.starts_with("G6a content custody") && err.contains("record 2"), "{err}");
    }

    #[test]
    fn g6a_rejects_data_written_after_recorder() {
        let d1 = id(1, 0);
        let v1 = ref_batch(&[(d1, b"payload")]);
        let disk = vec![
            incarnation_record(1),
            recorder_record(0, Some(v1), None, None),
            data_record(d1, Bytes::from_static(b"payload")),
        ];
        let err = check_disks(&epochs(vec![0]), &[&disk], &BTreeMap::new()).unwrap_err();
        assert!(err.starts_with("G6a content custody"), "{err}");
        assert!(err.contains("record 1"), "{err}");
    }

    #[test]
    fn g6a_rejects_mismatched_data_bytes() {
        let d1 = id(1, 0);
        let v1 = ref_batch(&[(d1, b"expected")]);
        let disk =
            vec![data_record(d1, Bytes::from_static(b"different")), recorder_record(0, Some(v1), None, None)];
        let err = check_disks(&epochs(vec![0]), &[&disk], &BTreeMap::new()).unwrap_err();
        assert!(err.starts_with("G6a content custody") && err.contains("record 1"), "{err}");
    }

    #[test]
    fn g6a_allows_decided_without_local_data_on_single_disk() {
        let d1 = id(1, 0);
        let v1 = ref_batch(&[(d1, b"batch")]);
        let disk = vec![decided_record(0, v1)];
        let parsed = inspect::custody(&disk).unwrap();
        assert_eq!(parsed.decided.get(&0), Some(&vec![d1]));
        assert!(parsed.data.is_empty());
    }

    #[test]
    fn g6b_requires_majority_durable_data_or_floor() {
        let d1 = id(1, 0);
        let b1 = b"batch-data";
        let v1 = ref_batch(&[(d1, b1)]);
        let e = epochs(vec![0, 1, 2]);
        let decided_only = vec![decided_record(0, v1.clone())];
        let with_data = vec![data_record(d1, Bytes::from_static(b1)), decided_record(0, v1.clone())];
        let empty: Vec<Bytes> = Vec::new();

        // Only 1 of 3 holds Data -> must fail G6b.
        let err = check_disks(&e, &[&with_data, &decided_only, &empty], &BTreeMap::new()).unwrap_err();
        assert!(err.starts_with("G6b content custody"), "{err}");

        // 2 of 3 hold Data -> passes.
        assert!(check_disks(&e, &[&with_data, &with_data, &decided_only], &BTreeMap::new()).is_ok());

        // 1 holds Data + 1 compacted through slot 0 (floor = 1 > 0) -> 2 of 3 -> passes.
        let compacted0 = vec![compacted_record(0)];
        assert!(check_disks(&e, &[&with_data, &compacted0, &decided_only], &BTreeMap::new()).is_ok());

        // Boundary: slot 1 decided, compacted through slot 0 (floor = 1, not > 1) -> fails.
        let dec1_only = vec![decided_record(1, v1.clone())];
        let with_data1 = vec![data_record(d1, Bytes::from_static(b1)), decided_record(1, v1)];
        let err = check_disks(&e, &[&with_data1, &compacted0, &dec1_only], &BTreeMap::new()).unwrap_err();
        assert!(err.starts_with("G6b content custody"), "{err}");
    }

    #[test]
    fn g6b_requires_every_ref_of_slot_at_each_covering_member() {
        let d1 = id(1, 0);
        let d2 = id(2, 0);
        let v = ref_batch(&[(d1, b"aaa"), (d2, b"bbb")]);
        let e = epochs(vec![0, 1, 2]);
        let n0 = vec![
            data_record(d1, Bytes::from_static(b"aaa")),
            data_record(d2, Bytes::from_static(b"bbb")),
            decided_record(0, v),
        ];
        let n1 = vec![data_record(d1, Bytes::from_static(b"aaa"))];
        let n2 = vec![data_record(d2, Bytes::from_static(b"bbb"))];
        // Only n0 holds both d1 and d2 -> minority -> must fail G6b.
        let err = check_disks(&e, &[&n0, &n1, &n2], &BTreeMap::new()).unwrap_err();
        assert!(err.starts_with("G6b content custody"), "{err}");
    }

    #[test]
    fn g6b_uses_slot_epoch_members() {
        let d1 = id(1, 0);
        let b1 = b"epoch-batch";
        let v = ref_batch(&[(d1, b1)]);
        let mut e = epochs(vec![0, 1, 2]);
        e.applied.insert(1, (5, vec![2, 3, 4]));

        let has = vec![data_record(d1, Bytes::from_static(b1))];
        let dec = vec![decided_record(5, v)];
        let empty: Vec<Bytes> = Vec::new();

        // Old members 0, 1 hold data, only 2 of new members [2, 3, 4] holds it -> fails G6b.
        let err = check_disks(&e, &[&has, &has, &has, &dec, &empty], &BTreeMap::new()).unwrap_err();
        assert!(err.starts_with("G6b content custody"), "{err}");

        // New members 3 and 4 hold data (2 of 3 in [2, 3, 4]) -> passes even if 0, 1, 2 lack it.
        assert!(check_disks(&e, &[&empty, &empty, &dec, &has, &has], &BTreeMap::new()).is_ok());
    }

    #[test]
    fn g6b_detects_minority_via_history_decided_before_decided_record_persists() {
        let d1 = id(1, 0);
        let b1 = b"in-flight-decision";
        let v = ref_batch(&[(d1, b1)]);
        let e = epochs(vec![0, 1, 2]);
        let mut decided = BTreeMap::new();
        decided.insert(0, proposal_value(0, v.clone()));

        let rec_disk = vec![data_record(d1, Bytes::from_static(b1)), recorder_record(0, Some(v), None, None)];
        let empty: Vec<Bytes> = Vec::new();

        // Only n0 has Data+Recorder (no Decided record anywhere yet) -> minority -> fails G6b.
        let err = check_disks(&e, &[&rec_disk, &empty, &empty], &decided).unwrap_err();
        assert!(err.starts_with("G6b content custody"), "{err}");

        // n0 and n1 both have Data+Recorder -> majority -> passes.
        assert!(check_disks(&e, &[&rec_disk, &rec_disk, &empty], &decided).is_ok());
    }

    #[test]
    fn g6b_catches_checkpoint_that_drops_referenced_data() {
        let d1 = id(1, 0);
        let b1 = b"checkpoint-data";
        let v = ref_batch(&[(d1, b1)]);
        let e = epochs(vec![0, 1, 2]);
        let with_data = vec![data_record(d1, Bytes::from_static(b1)), decided_record(0, v.clone())];
        let dropped = vec![decided_record(0, v)];

        let mut c = Custody::new(3);
        c.on_replace(0, &with_data).unwrap();
        c.on_replace(1, &with_data).unwrap();
        c.on_replace(2, &dropped).unwrap();
        c.check_decided(&e, &BTreeMap::new()).unwrap();

        // n1 replaces its disk with a checkpoint that omits Data without advancing floor.
        c.on_replace(1, &dropped).unwrap();
        let err = c.check_decided(&e, &BTreeMap::new()).unwrap_err();
        assert!(err.starts_with("G6b content custody"), "{err}");
    }

    #[test]
    fn inline_batch_bytes_draws_all_three_values_across_seeds() {
        let mut seen = BTreeSet::new();
        for seed in 0..20 {
            for node in 0..3 {
                seen.insert(crate::config::inline_batch_bytes(seed, node));
            }
        }
        assert_eq!(seen, BTreeSet::from([64, 256, u32::MAX]));
    }
}

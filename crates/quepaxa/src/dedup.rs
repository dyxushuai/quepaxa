//! Commit-time deduplication: an entry may be decided twice (forwarded to the leader and
//! later hedged by its origin), but is applied once. The state evolves only with the
//! decided log, so every replica filters identically.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use bytes::BufMut;

use crate::codec::{DecodeError, Reader};

use crate::types::{NodeId, ProposalId};

/// Incarnations remembered per origin; entries from older ones are treated as seen.
const INCARNATIONS_KEPT: usize = 4;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
struct Window {
    /// Every id below this was seen.
    below: u64,
    /// Seen ids at or above `below`.
    above: BTreeSet<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(crate) struct Dedup {
    origins: BTreeMap<NodeId, BTreeMap<u64, Window>>,
}

impl Dedup {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.put_u32_le(self.origins.len() as u32);
        for (origin, incs) in &self.origins {
            out.put_u32_le(origin.0);
            out.put_u32_le(incs.len() as u32);
            for (inc, w) in incs {
                out.put_u64_le(*inc);
                out.put_u64_le(w.below);
                out.put_u32_le(w.above.len() as u32);
                for id in &w.above {
                    out.put_u64_le(*id);
                }
            }
        }
    }

    pub fn decode(r: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let mut origins = BTreeMap::new();
        for _ in 0..r.u32()? {
            let origin = NodeId(r.u32()?);
            let mut incs = BTreeMap::new();
            for _ in 0..r.u32()? {
                let inc = r.u64()?;
                let below = r.u64()?;
                let mut above = BTreeSet::new();
                for _ in 0..r.u32()? {
                    let id = r.u64()?;
                    // Ids stop below u64::MAX (batch::decode), so `below` never overflows.
                    if id <= below || id == u64::MAX {
                        return Err(DecodeError::BadTag);
                    }
                    above.insert(id);
                }
                incs.insert(inc, Window { below, above });
            }
            if incs.len() > INCARNATIONS_KEPT {
                return Err(DecodeError::BadTag);
            }
            origins.insert(origin, incs);
        }
        Ok(Self { origins })
    }

    /// Whether the entry was already seen (without recording it).
    pub fn contains(&self, origin: NodeId, incarnation: u64, ProposalId(id): ProposalId) -> bool {
        let Some(incs) = self.origins.get(&origin) else { return false };
        match incs.get(&incarnation) {
            Some(w) => id < w.below || w.above.contains(&id),
            None => incs.len() >= INCARNATIONS_KEPT && incs.keys().next().is_some_and(|&o| incarnation < o),
        }
    }

    /// Records the entry; true the first time it is seen.
    pub fn first_time(&mut self, origin: NodeId, incarnation: u64, ProposalId(id): ProposalId) -> bool {
        let incs = self.origins.entry(origin).or_default();
        if incs.len() >= INCARNATIONS_KEPT && !incs.contains_key(&incarnation) {
            if incs.keys().next().is_some_and(|&oldest| incarnation < oldest) {
                return false;
            }
            incs.pop_first();
        }
        let w = incs.entry(incarnation).or_default();
        if id < w.below || !w.above.insert(id) {
            return false;
        }
        while w.above.remove(&w.below) {
            w.below += 1;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(d: &mut Dedup, inc: u64, id: u64) -> bool {
        !d.first_time(NodeId(1), inc, ProposalId(id))
    }

    #[test]
    fn second_occurrence_is_filtered_in_any_order() {
        let mut d = Dedup::default();
        assert!(!seen(&mut d, 1, 2));
        assert!(!seen(&mut d, 1, 0));
        assert!(seen(&mut d, 1, 2));
        assert!(seen(&mut d, 1, 0));
        assert!(!seen(&mut d, 1, 1));
        assert!(seen(&mut d, 1, 1));
    }

    #[test]
    fn contiguous_ids_compact_into_the_watermark() {
        let mut d = Dedup::default();
        for id in 0..1000 {
            d.first_time(NodeId(1), 1, ProposalId(id));
        }
        let w = &d.origins[&NodeId(1)][&1];
        assert_eq!((w.below, w.above.len()), (1000, 0));
    }

    #[test]
    fn incarnations_and_origins_are_independent() {
        let mut d = Dedup::default();
        assert!(!seen(&mut d, 1, 0));
        assert!(!seen(&mut d, 2, 0));
        assert!(d.first_time(NodeId(2), 1, ProposalId(0)));
    }

    #[test]
    fn only_a_full_window_forgets_older_incarnations() {
        let mut d = Dedup::default();
        assert!(!seen(&mut d, 5, 0));
        assert!(!d.contains(NodeId(1), 3, ProposalId(0)));
        assert!(!seen(&mut d, 3, 0));
        for inc in 6..=7 {
            assert!(!seen(&mut d, inc, 0));
        }
        assert!(d.contains(NodeId(1), 2, ProposalId(0)));
        assert!(!d.contains(NodeId(1), 4, ProposalId(0)));
        assert!(!seen(&mut d, 8, 0));
        assert_eq!(d.origins[&NodeId(1)].len(), INCARNATIONS_KEPT);
        assert!(d.contains(NodeId(1), 3, ProposalId(9)));
    }

    #[test]
    fn entries_from_forgotten_incarnations_count_as_seen() {
        let mut d = Dedup::default();
        for inc in 1..=5 {
            d.first_time(NodeId(1), inc, ProposalId(0));
        }
        assert!(seen(&mut d, 1, 7));
        assert!(!seen(&mut d, 6, 0));
    }

    #[test]
    fn decode_rejects_the_largest_id() {
        let mut d = Dedup::default();
        d.first_time(NodeId(1), 1, ProposalId(5));
        let mut bytes = Vec::new();
        d.encode(&mut bytes);
        assert_eq!(Dedup::decode(&mut Reader::new(&bytes::Bytes::from(bytes.clone()))), Ok(d));
        // The last 8 bytes are the one id above the watermark.
        let at = bytes.len() - 8;
        bytes[at..].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(Dedup::decode(&mut Reader::new(&bytes::Bytes::from(bytes))), Err(DecodeError::BadTag));
    }
}

//! Leader schedule, derived only from the decided log so every replica agrees on the
//! leader of every slot without extra messages.
//!
//! The schedule in force for slot `k` is the latest one decided at a slot `≤ k − LAG`.
//! A replica only drives slots below `commit_index + max_in_flight_slots ≤ commit_index +
//! LAG`, so the schedule of any slot it touches is already fixed by its decided prefix.
//! When `ROTATE_AFTER` consecutive slots are decided with values that did not originate at
//! their leader (it is down, partitioned or slow), the leader moves to the back.

use alloc::vec::Vec;
use bytes::BufMut;

use crate::codec::{DecodeError, Reader};

use crate::types::{NodeId, Slot};

/// Slots between deciding a schedule change and it taking effect.
pub(crate) const LAG: u64 = 64;
/// Consecutive slots not led by their leader before it is rotated out.
const ROTATE_AFTER: u32 = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(crate) struct Schedule {
    /// `(first slot it applies to, order)`, ascending; the first entry starts at slot 0.
    history: Vec<(u64, Vec<NodeId>)>,
    streak: u32,
    /// No new rotation is counted until the last one takes effect.
    quiet_until: u64,
}

impl Schedule {
    #[cfg(test)]
    pub fn new(members: &[NodeId]) -> Self {
        let mut order = members.to_vec();
        order.sort_unstable();
        Self::with_order(order)
    }

    /// A schedule whose first leaders are `order`, first to last.
    pub fn with_order(order: Vec<NodeId>) -> Self {
        Self { history: alloc::vec![(0, order)], streak: 0, quiet_until: 0 }
    }

    fn order(&self, slot: Slot) -> &[NodeId] {
        let i = self.history.partition_point(|(from, _)| *from <= slot.0);
        &self.history[i.saturating_sub(1)].1
    }

    pub fn leader(&self, slot: Slot) -> NodeId {
        self.order(slot)[0]
    }

    /// 0 for the leader, then the hedging order; a non-member ranks last.
    pub fn position(&self, node: NodeId, slot: Slot) -> u64 {
        let order = self.order(slot);
        order.iter().position(|&n| n == node).unwrap_or(order.len()) as u64
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.put_u32_le(self.streak);
        out.put_u64_le(self.quiet_until);
        out.put_u32_le(self.history.len() as u32);
        for (from, order) in &self.history {
            out.put_u64_le(*from);
            out.put_u32_le(order.len() as u32);
            for n in order {
                out.put_u32_le(n.0);
            }
        }
    }

    /// Restores a schedule; `members` must be the same set the encoder had.
    pub fn decode(r: &mut Reader<'_>, members: &[NodeId]) -> Result<Self, DecodeError> {
        let streak = r.u32()?;
        if streak >= ROTATE_AFTER {
            return Err(DecodeError::BadTag);
        }
        let quiet_until = r.u64()?;
        let n = r.u32()? as usize;
        let mut history = Vec::new();
        for _ in 0..n {
            let from = r.u64()?;
            let len = r.u32()? as usize;
            if len != members.len() {
                return Err(DecodeError::BadTag);
            }
            let mut order = Vec::with_capacity(len);
            for _ in 0..len {
                let node = NodeId(r.u32()?);
                if !members.contains(&node) || order.contains(&node) {
                    return Err(DecodeError::BadTag);
                }
                order.push(node);
            }
            history.push((from, order));
        }
        if history.is_empty() || !history.is_sorted_by_key(|(f, _)| *f) {
            return Err(DecodeError::BadTag);
        }
        Ok(Self { history, streak, quiet_until })
    }

    /// Feeds each decided slot exactly once, in slot order, with its value's origin.
    pub fn observe(&mut self, slot: Slot, origin: NodeId) {
        if slot.0 < self.quiet_until {
            return;
        }
        if origin == self.leader(slot) {
            self.streak = 0;
            return;
        }
        self.streak += 1;
        if self.streak >= ROTATE_AFTER {
            let mut order = self.order(slot).to_vec();
            order.rotate_left(1);
            let from = slot.0 + LAG;
            self.history.push((from, order));
            self.streak = 0;
            self.quiet_until = from;
            // Older schedules can no longer apply to any slot a replica will drive.
            let keep_from = self.history.partition_point(|(f, _)| *f <= slot.0).saturating_sub(1);
            self.history.drain(..keep_from);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[u32]) -> Vec<NodeId> {
        v.iter().copied().map(NodeId).collect()
    }

    #[test]
    fn initial_leader_is_the_smallest_member() {
        let s = Schedule::new(&ids(&[3, 1, 2]));
        assert_eq!((s.leader(Slot(0)), s.position(NodeId(3), Slot(0))), (NodeId(1), 2));
    }

    #[test]
    fn eight_missed_slots_rotate_the_leader_after_the_lag() {
        let mut s = Schedule::new(&ids(&[0, 1, 2]));
        for slot in 0..8 {
            s.observe(Slot(slot), NodeId(1));
        }
        assert_eq!(s.leader(Slot(7 + LAG - 1)), NodeId(0));
        assert_eq!(s.leader(Slot(7 + LAG)), NodeId(1));
        assert_eq!(s.position(NodeId(0), Slot(7 + LAG)), 2);
    }

    #[test]
    fn a_slot_led_by_its_leader_resets_the_streak() {
        let mut s = Schedule::new(&ids(&[0, 1, 2]));
        for slot in 0..7 {
            s.observe(Slot(slot), NodeId(2));
        }
        s.observe(Slot(7), NodeId(0));
        for slot in 8..15 {
            s.observe(Slot(slot), NodeId(2));
        }
        assert_eq!(s.leader(Slot(200)), NodeId(0));
    }

    #[test]
    fn misses_before_a_rotation_takes_effect_do_not_rotate_again() {
        let mut s = Schedule::new(&ids(&[0, 1, 2]));
        for slot in 0..(8 + LAG) {
            s.observe(Slot(slot), NodeId(2));
        }
        assert_eq!(s.leader(Slot(8 + LAG)), NodeId(1));
    }

    #[test]
    fn decode_rejects_an_empty_or_unsorted_history() {
        let members = ids(&[0, 1, 2]);
        let encoded = |froms: &[u64]| {
            let mut out = Vec::new();
            out.put_u32_le(0);
            out.put_u64_le(0);
            out.put_u32_le(froms.len() as u32);
            for from in froms {
                out.put_u64_le(*from);
                out.put_u32_le(3);
                [0, 1, 2].into_iter().for_each(|n| out.put_u32_le(n));
            }
            bytes::Bytes::from(out)
        };
        let decode =
            |b: &bytes::Bytes| Schedule::decode(&mut Reader::new(b), &members).map(|s| s.history.len());
        assert_eq!(decode(&encoded(&[0, 9])), Ok(2));
        assert!(decode(&encoded(&[])).is_err());
        assert!(decode(&encoded(&[9, 0])).is_err());
    }

    proptest::proptest! {
        // Replicas that observe the same decided prefix agree on every slot's leader.
        #[test]
        fn same_log_same_leaders(origins in proptest::collection::vec(0u32..3, 0..400)) {
            let (mut a, mut b) = (Schedule::new(&ids(&[0, 1, 2])), Schedule::new(&ids(&[2, 1, 0])));
            for (slot, o) in origins.iter().enumerate() {
                a.observe(Slot(slot as u64), NodeId(*o));
                b.observe(Slot(slot as u64), NodeId(*o));
            }
            for k in 0..(origins.len() as u64 + LAG) {
                proptest::prop_assert_eq!(a.leader(Slot(k)), b.leader(Slot(k)));
            }
        }
    }
}

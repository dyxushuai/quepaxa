//! Leader schedule, derived only from the decided log so every replica agrees on the
//! leader of every slot without extra messages.
//!
//! The schedule in force for slot `k` is the latest one decided at a slot `≤ k − LAG`.
//! A replica only drives slots below `commit_index + max_in_flight_slots ≤ commit_index +
//! LAG`, so the schedule of any slot it touches is already fixed by its decided prefix.
//! When `ROTATE_AFTER` consecutive slots are decided with values that did not originate at
//! their leader (it is down, partitioned or slow), the leader moves to the back.
//!
//! Membership changes ride the same mechanism: an order is always the slot's members,
//! preferred leaders first, so a membership change decided in slot `s` is a new order from
//! `s + LAG` (a new epoch). The same window bound then guarantees that every proposer of a
//! slot uses the same recorders and majority.

use alloc::vec::Vec;
use bytes::BufMut;

use crate::codec::{DecodeError, Reader};

use crate::types::{NodeId, ProposalId, Slot};

/// Slots between deciding a schedule change and it taking effect.
pub(crate) const LAG: u64 = 64;
/// Consecutive slots not led by their leader before it is rotated out.
const ROTATE_AFTER: u32 = 8;
/// Upper bound on members in one order (keeps decoding allocations bounded).
pub(crate) const MAX_MEMBERS: usize = 1024;

/// Identity of a proposed membership change: `(origin, incarnation, id)`.
pub(crate) type ChangeKey = (NodeId, u64, ProposalId);

/// Encodes a membership change's new order (members, preferred leaders first).
pub(crate) fn encode_order(order: &[NodeId]) -> bytes::Bytes {
    let mut out = Vec::with_capacity(4 + 4 * order.len());
    out.put_u32_le(order.len() as u32);
    for n in order {
        out.put_u32_le(n.0);
    }
    bytes::Bytes::from(out)
}

/// Decodes an order written by [`encode_order`]: non-empty, without duplicates.
pub(crate) fn decode_order(b: &bytes::Bytes) -> Option<Vec<NodeId>> {
    let mut r = Reader::new(b);
    let len = r.u32().ok()? as usize;
    if len == 0 || len > MAX_MEMBERS {
        return None;
    }
    let mut order = Vec::with_capacity(len);
    for _ in 0..len {
        let n = NodeId(r.u32().ok()?);
        if order.contains(&n) {
            return None;
        }
        order.push(n);
    }
    r.finish().ok()?;
    Some(order)
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(crate) struct Schedule {
    /// `(first slot it applies to, order)`, ascending; the first entry starts at slot 0.
    history: Vec<(u64, Vec<NodeId>)>,
    streak: u32,
    /// No new rotation is counted until the last one takes effect.
    quiet_until: u64,
    /// Membership epoch in force from `epoch_from` (0: the configured members).
    epoch: u64,
    epoch_from: u64,
    /// Changes already applied or ignored, so another copy decided later (hedging may
    /// include one twice) is ignored too and never reverts a newer membership.
    settled: Vec<ChangeKey>,
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
        Self {
            history: alloc::vec![(0, order)],
            streak: 0,
            quiet_until: 0,
            epoch: 0,
            epoch_from: 0,
            settled: Vec::new(),
        }
    }

    fn order(&self, slot: Slot) -> &[NodeId] {
        let i = self.history.partition_point(|(from, _)| *from <= slot.0);
        &self.history[i.saturating_sub(1)].1
    }

    pub fn leader(&self, slot: Slot) -> NodeId {
        self.order(slot)[0]
    }

    /// The members of `slot`'s epoch: its recorders, preferred leaders first.
    pub fn members(&self, slot: Slot) -> &[NodeId] {
        self.order(slot)
    }

    /// Whether `node` is a member of any epoch still in the schedule (current or pending).
    pub fn knows(&self, node: NodeId) -> bool {
        self.history.iter().any(|(_, order)| order.contains(&node))
    }

    /// Everyone in any epoch still in the schedule, without duplicates.
    pub fn everyone(&self) -> Vec<NodeId> {
        let mut all: Vec<NodeId> = self.history.iter().flat_map(|(_, o)| o.iter().copied()).collect();
        all.sort_unstable();
        all.dedup();
        all
    }

    /// The latest epoch and the slot it starts at (possibly still in the future).
    pub fn epoch(&self) -> (u64, u64) {
        (self.epoch, self.epoch_from)
    }

    /// Applies membership change `key`, decided in `slot`: from `slot + LAG` on, `order`
    /// (the new members, preferred leaders first) leads. Returns the new epoch and its first
    /// slot, or `None` if ignored because another change has not taken effect yet or this
    /// one was already applied or ignored. Feed it in slot order, after `observe` for the
    /// same slot.
    pub fn reconfigure(&mut self, slot: Slot, key: ChangeKey, order: Vec<NodeId>) -> Option<(u64, u64)> {
        if self.settled.contains(&key) {
            return None;
        }
        self.settled.push(key);
        if slot.0 < self.epoch_from {
            return None;
        }
        let from = slot.0 + LAG;
        // A rotation counted in this very slot is superseded.
        if self.history.last().is_some_and(|(f, _)| *f == from) {
            self.history.pop();
        }
        self.history.push((from, order));
        self.epoch += 1;
        self.epoch_from = from;
        self.streak = 0;
        self.quiet_until = from;
        self.prune(slot);
        Some((self.epoch, from))
    }

    /// Older schedules can no longer apply to any slot a replica will drive.
    fn prune(&mut self, slot: Slot) {
        let keep_from = self.history.partition_point(|(f, _)| *f <= slot.0).saturating_sub(1);
        self.history.drain(..keep_from);
    }

    /// 0 for the leader, then the hedging order; a non-member ranks last.
    pub fn position(&self, node: NodeId, slot: Slot) -> u64 {
        let order = self.order(slot);
        order.iter().position(|&n| n == node).unwrap_or(order.len()) as u64
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.put_u64_le(self.epoch);
        out.put_u64_le(self.epoch_from);
        out.put_u32_le(self.settled.len() as u32);
        for (origin, incarnation, id) in &self.settled {
            out.put_u32_le(origin.0);
            out.put_u64_le(*incarnation);
            out.put_u64_le(id.0);
        }
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

    /// Restores a schedule; until the first change every order is a permutation of the
    /// configured `members`.
    pub fn decode(r: &mut Reader<'_>, members: &[NodeId]) -> Result<Self, DecodeError> {
        let epoch = r.u64()?;
        let epoch_from = r.u64()?;
        let n = r.u32()? as usize;
        let mut settled = Vec::new();
        for _ in 0..n {
            settled.push((NodeId(r.u32()?), r.u64()?, ProposalId(r.u64()?)));
        }
        if (settled.len() as u64) < epoch {
            return Err(DecodeError::BadTag);
        }
        let streak = r.u32()?;
        if streak >= ROTATE_AFTER {
            return Err(DecodeError::BadTag);
        }
        let quiet_until = r.u64()?;
        let n = r.u32()? as usize;
        let mut history = Vec::new();
        for _ in 0..n {
            let from = r.u64()?;
            // Until the first change, every order is a permutation of the configured members.
            let configured = epoch == 0;
            let len = r.u32()? as usize;
            if len == 0 || len > MAX_MEMBERS || (configured && len != members.len()) {
                return Err(DecodeError::BadTag);
            }
            let mut order = Vec::with_capacity(len);
            for _ in 0..len {
                let node = NodeId(r.u32()?);
                if (configured && !members.contains(&node)) || order.contains(&node) {
                    return Err(DecodeError::BadTag);
                }
                order.push(node);
            }
            history.push((from, order));
        }
        if history.is_empty() || !history.is_sorted_by_key(|(f, _)| *f) {
            return Err(DecodeError::BadTag);
        }
        Ok(Self { history, streak, quiet_until, epoch, epoch_from, settled })
    }

    /// Whether this could be the schedule after feeding every slot below `next`: the
    /// invariant verification/verus/schedule.rs proves the window rule under. Entries start
    /// in increasing slots, the first at or before the last slot fed and every one at most
    /// `LAG` after it.
    pub fn fits(&self, next: u64) -> bool {
        let increasing = self.history.windows(2).all(|w| w[0].0 < w[1].0);
        match next.checked_sub(1) {
            Some(fed) => {
                increasing
                    && self.history[0].0 <= fed
                    && self.history.iter().all(|(from, _)| *from <= fed.saturating_add(LAG))
            }
            None => self.history.len() == 1 && self.history[0].0 == 0,
        }
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
            self.prune(slot);
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
            out.put_u64_le(0);
            out.put_u64_le(0);
            out.put_u32_le(0);
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

    fn key(id: u64) -> ChangeKey {
        (NodeId(0), 1, ProposalId(id))
    }

    #[test]
    fn a_membership_change_takes_effect_a_window_later() {
        let mut s = Schedule::new(&ids(&[0, 1, 2]));
        assert_eq!(s.reconfigure(Slot(10), key(1), ids(&[3, 1, 2])), Some((1, 10 + LAG)));
        assert_eq!(s.members(Slot(10 + LAG - 1)), ids(&[0, 1, 2]));
        assert_eq!((s.members(Slot(10 + LAG)), s.leader(Slot(10 + LAG))), (&ids(&[3, 1, 2])[..], NodeId(3)));
        assert!(s.knows(NodeId(3)) && s.knows(NodeId(0)));
        assert_eq!(s.everyone(), ids(&[0, 1, 2, 3]));
    }

    #[test]
    fn one_change_at_a_time_and_never_twice() {
        let mut s = Schedule::new(&ids(&[0, 1, 2]));
        s.reconfigure(Slot(10), key(1), ids(&[1, 2, 3])).unwrap();
        assert_eq!(s.reconfigure(Slot(11), key(2), ids(&[0, 1])), None, "the first is still pending");
        assert_eq!(s.reconfigure(Slot(10 + LAG), key(1), ids(&[1, 2, 3])), None, "a late duplicate");
        assert_eq!(
            s.reconfigure(Slot(10 + LAG), key(2), ids(&[0, 1])),
            None,
            "a late copy of an ignored one"
        );
        assert_eq!(s.reconfigure(Slot(10 + LAG), key(3), ids(&[0, 1])), Some((2, 10 + 2 * LAG)));
        assert_eq!(s.epoch(), (2, 10 + 2 * LAG));
    }

    #[test]
    fn a_change_supersedes_a_rotation_counted_in_the_same_slot() {
        let mut s = Schedule::new(&ids(&[0, 1, 2]));
        for slot in 0..8 {
            s.observe(Slot(slot), NodeId(1));
        }
        s.reconfigure(Slot(7), key(1), ids(&[2, 0])).unwrap();
        assert_eq!(s.members(Slot(7 + LAG)), ids(&[2, 0]));
        // Rotations resume in the new epoch, counted from its own leader.
        for slot in (7 + LAG)..(7 + LAG + 8) {
            s.observe(Slot(slot), NodeId(0));
        }
        assert_eq!(s.leader(Slot(7 + 2 * LAG + 7)), NodeId(0));
    }

    #[test]
    fn epochs_round_trip() {
        let mut s = Schedule::new(&ids(&[0, 1, 2]));
        s.reconfigure(Slot(3), key(9), ids(&[4, 5, 6, 7])).unwrap();
        let mut out = Vec::new();
        s.encode(&mut out);
        let b = bytes::Bytes::from(out);
        assert_eq!(Schedule::decode(&mut Reader::new(&b), &ids(&[0, 1, 2])), Ok(s));
    }

    #[test]
    fn orders_decode_only_when_well_formed() {
        assert_eq!(decode_order(&encode_order(&ids(&[2, 0]))), Some(ids(&[2, 0])));
        assert_eq!(decode_order(&encode_order(&[])), None);
        assert_eq!(decode_order(&encode_order(&ids(&[1, 1]))), None);
        assert_eq!(decode_order(&bytes::Bytes::from_static(&[1, 0, 0, 0])), None);
    }

    #[test]
    fn orders_are_bounded_in_size() {
        let at_most = (0..MAX_MEMBERS as u32).map(NodeId).collect::<Vec<_>>();
        assert!(decode_order(&encode_order(&at_most)).is_some());
        let too_many = (0..=MAX_MEMBERS as u32).map(NodeId).collect::<Vec<_>>();
        assert_eq!(decode_order(&encode_order(&too_many)), None);
    }

    #[test]
    fn old_schedules_are_pruned() {
        let mut s = Schedule::new(&ids(&[0, 1, 2]));
        for slot in 0..10_000 {
            s.observe(Slot(slot), NodeId(9));
        }
        assert!(s.history.len() <= 2, "{} orders kept", s.history.len());
    }

    /// One schedule encoding with a single order: before epochs (`None`) or in `epoch`.
    fn encoded(epoch: u64, order: &[u32]) -> bytes::Bytes {
        let mut out = Vec::new();
        out.put_u64_le(epoch);
        out.put_u64_le(0);
        out.put_u32_le(epoch as u32);
        for k in 0..epoch {
            out.put_u32_le(0);
            out.put_u64_le(1);
            out.put_u64_le(k);
        }
        out.put_u32_le(0);
        out.put_u64_le(0);
        out.put_u32_le(1);
        out.put_u64_le(0);
        out.put_u32_le(order.len() as u32);
        order.iter().for_each(|n| out.put_u32_le(*n));
        bytes::Bytes::from(out)
    }

    #[test]
    fn decoding_checks_every_order() {
        let members = ids(&[0, 1, 2]);
        let decode = |epoch: u64, order: &[u32]| {
            Schedule::decode(&mut Reader::new(&encoded(epoch, order)), &members).is_ok()
        };
        // Before any change, orders are permutations of the configured members.
        assert!(decode(0, &[2, 0, 1]));
        assert!(!decode(0, &[0, 1]) && !decode(0, &[0, 1, 7]) && !decode(0, &[0, 1, 1]));
        // After one, any non-empty order of distinct replicas, up to the bound.
        assert!(decode(1, &[7, 8]) && decode(1, &[5]));
        assert!(!decode(1, &[]) && !decode(1, &[7, 7]));
        let max: Vec<u32> = (0..MAX_MEMBERS as u32).collect();
        let over: Vec<u32> = (0..=MAX_MEMBERS as u32).collect();
        assert!(decode(1, &max) && !decode(1, &over));
        // An epoch needs at least as many settled changes.
        let mut short = encoded(2, &[5]).to_vec();
        short[16..20].copy_from_slice(&1u32.to_le_bytes());
        short.drain(20..40);
        let short = bytes::Bytes::from(short);
        assert!(Schedule::decode(&mut Reader::new(&short), &members).is_err());
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

    #[test]
    fn fits_only_what_feeding_the_slots_below_next_can_build() {
        let s = Schedule::new(&ids(&[0, 1, 2]));
        assert!(s.fits(0) && s.fits(10));
        // The first order starting after the last slot fed: a change at slot 10 would then
        // replace the only order, and slot 11 would change members at once.
        let ahead = Schedule { history: alloc::vec![(74, ids(&[0, 1, 2]))], ..s.clone() };
        assert!(!ahead.fits(10) && ahead.fits(75));
        let tied = Schedule {
            history: alloc::vec![(0, ids(&[0, 1, 2])), (5, ids(&[1, 2, 0])), (5, ids(&[2, 0, 1]))],
            ..s.clone()
        };
        assert!(!tied.fits(10));
        let far = Schedule { history: alloc::vec![(0, ids(&[0, 1, 2])), (10 + LAG, ids(&[1, 2, 0]))], ..s };
        assert!(!far.fits(10) && far.fits(11));
    }
}

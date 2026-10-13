//! Persist-before-send (ADR 0002): records written by the replica, and the sends and
//! effects that must not happen before those records are durable.
//!
//! Every write goes out in the next [`Action::Persist`], which carries the sends that wait
//! for it. Work that only waits for writes already in flight is parked behind the last one,
//! so idle replies never cost an extra (empty) write. Nothing leaves before the
//! incarnation record, the first write, is durable.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use bytes::Bytes;
use core::mem;

use crate::api::{Action, Error, Outbound};
use crate::types::PersistId;

#[cfg_attr(feature = "model", derive(Clone, Hash))]
struct InFlight<E> {
    id: u64,
    effects: Vec<E>,
    parked: Vec<Outbound>,
}

#[cfg_attr(feature = "model", derive(Clone, Hash))]
pub(super) struct Gate<E> {
    next_persist: u64,
    acked: Option<u64>,
    writes: Vec<Bytes>,
    /// The next `Persist` replaces all earlier records.
    replace: bool,
    gated: Vec<Outbound>,
    effects: Vec<E>,
    in_flight: VecDeque<InFlight<E>>,
}

impl<E> Gate<E> {
    /// A gate for a new incarnation whose first durable record is `incarnation`.
    pub fn new(incarnation: Bytes) -> Self {
        Self {
            next_persist: 0,
            acked: None,
            writes: alloc::vec![incarnation],
            replace: false,
            gated: Vec::new(),
            effects: Vec::new(),
            in_flight: VecDeque::new(),
        }
    }

    /// Whether the incarnation is durable, so sends may leave at all.
    fn ready(&self) -> bool {
        self.acked.is_some()
    }

    /// Whether everything written so far is durable.
    fn idle(&self) -> bool {
        self.ready() && self.writes.is_empty() && self.in_flight.is_empty()
    }

    pub fn write(&mut self, record: Bytes) {
        self.writes.push(record);
    }

    /// Replaces every earlier record with `records`, which must reflect all writes so far.
    pub fn checkpoint(&mut self, records: Vec<Bytes>) {
        self.writes = records;
        self.replace = true;
    }

    /// A send that does not depend on durable state.
    pub fn send(&mut self, out: Outbound, actions: &mut Vec<Action>) {
        if self.ready() {
            actions.push(Action::Send(out));
        } else {
            self.gated.push(out);
        }
    }

    /// A send that reports state possibly not yet durable.
    pub fn send_after_durable(&mut self, out: Outbound, actions: &mut Vec<Action>) {
        if self.idle() {
            actions.push(Action::Send(out));
        } else {
            self.gated.push(out);
        }
    }

    /// A send that must wait for writes already made (and not yet durable).
    pub fn hold(&mut self, out: Outbound) {
        debug_assert!(!self.idle(), "held send without a pending write");
        self.gated.push(out);
    }

    /// Returns `e` if it may run now; otherwise it is returned by [`Gate::persisted`] once
    /// everything written so far is durable.
    #[must_use]
    pub fn after_durable(&mut self, e: E) -> Option<E> {
        if self.idle() {
            Some(e)
        } else {
            self.effects.push(e);
            None
        }
    }

    /// Issues a `Persist` for new writes, or parks waiting work behind the last one in flight.
    pub fn flush(&mut self, actions: &mut Vec<Action>) {
        if self.writes.is_empty() {
            self.park();
            debug_assert!(self.gated.is_empty() && self.effects.is_empty(), "gated work without a write");
            return;
        }
        let id = self.next_persist;
        self.next_persist += 1;
        actions.push(Action::Persist {
            id: PersistId(id),
            records: mem::take(&mut self.writes),
            then: mem::take(&mut self.gated),
            replace: mem::take(&mut self.replace),
        });
        self.in_flight.push_back(InFlight { id, effects: mem::take(&mut self.effects), parked: Vec::new() });
    }

    /// Parks work that waits only for writes already in flight behind the last of them.
    fn park(&mut self) {
        if self.writes.is_empty()
            && let Some(last) = self.in_flight.back_mut()
        {
            last.effects.append(&mut self.effects);
            last.parked.append(&mut self.gated);
        }
    }

    /// Persist `id` (and every earlier one) is durable: releases the sends parked behind
    /// them and returns their effects, in order.
    pub fn persisted(
        &mut self,
        PersistId(id): PersistId,
        actions: &mut Vec<Action>,
    ) -> Result<Vec<E>, Error> {
        if id >= self.next_persist || self.acked.is_some_and(|a| id <= a) {
            return Err(Error::UnknownPersistId);
        }
        self.acked = Some(id);
        self.park();
        let mut effects = Vec::new();
        while self.in_flight.front().is_some_and(|f| f.id <= id) {
            let Some(mut done) = self.in_flight.pop_front() else { break };
            actions.extend(done.parked.into_iter().map(Action::Send));
            effects.append(&mut done.effects);
        }
        Ok(effects)
    }

    /// Storage failed: waiting work never happens.
    pub fn fence(&mut self) {
        self.writes.clear();
        self.gated.clear();
        self.effects.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeId;

    fn out(n: u8) -> Outbound {
        Outbound { to: NodeId(1), bytes: Bytes::from(alloc::vec![n]) }
    }

    fn sent(actions: &[Action]) -> Vec<u8> {
        actions.iter().filter_map(|a| if let Action::Send(o) = a { Some(o.bytes[0]) } else { None }).collect()
    }

    fn persists(actions: &[Action]) -> Vec<(u64, usize, Vec<u8>)> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Persist { id, records, then, .. } => {
                    Some((id.0, records.len(), then.iter().map(|o| o.bytes[0]).collect()))
                }
                _ => None,
            })
            .collect()
    }

    fn started() -> (Gate<u32>, Vec<Action>) {
        let mut g = Gate::new(Bytes::from_static(b"inc"));
        let mut a = Vec::new();
        g.flush(&mut a);
        assert_eq!(g.persisted(PersistId(0), &mut a), Ok(Vec::new()));
        a.clear();
        (g, a)
    }

    #[test]
    fn nothing_leaves_before_the_incarnation_is_durable() {
        let mut g = Gate::new(Bytes::from_static(b"inc"));
        let mut a = Vec::new();
        g.send(out(1), &mut a);
        assert_eq!(g.after_durable(7), None);
        g.flush(&mut a);
        assert_eq!(persists(&a), [(0, 1, alloc::vec![1])]);
        assert!(sent(&a).is_empty());
        assert_eq!(g.persisted(PersistId(0), &mut a), Ok(alloc::vec![7]));
    }

    #[test]
    fn replies_ride_the_persist_of_the_state_they_report() {
        let (mut g, mut a) = started();
        g.write(Bytes::from_static(b"reg"));
        g.send_after_durable(out(2), &mut a);
        g.send(out(3), &mut a);
        g.flush(&mut a);
        assert_eq!(sent(&a), [3]);
        assert_eq!(persists(&a), [(1, 1, alloc::vec![2])]);
    }

    #[test]
    fn work_waiting_only_on_writes_in_flight_is_parked_without_a_new_persist() {
        let (mut g, mut a) = started();
        g.write(Bytes::from_static(b"reg"));
        g.flush(&mut a);
        a.clear();
        g.send_after_durable(out(4), &mut a);
        assert_eq!(g.after_durable(9), None);
        g.flush(&mut a);
        assert!(a.is_empty());
        assert_eq!(g.persisted(PersistId(1), &mut a), Ok(alloc::vec![9]));
        assert_eq!(sent(&a), [4]);
    }

    #[test]
    fn idle_storage_lets_replies_and_effects_through_at_once() {
        let (mut g, mut a) = started();
        g.send_after_durable(out(5), &mut a);
        assert_eq!(g.after_durable(1), Some(1));
        assert_eq!(sent(&a), [5]);
    }

    #[test]
    fn acknowledgements_must_be_issued_and_increasing() {
        let (mut g, mut a) = started();
        assert_eq!(g.persisted(PersistId(0), &mut a), Err(Error::UnknownPersistId));
        assert_eq!(g.persisted(PersistId(1), &mut a), Err(Error::UnknownPersistId));
    }

    #[test]
    fn one_acknowledgement_covers_every_earlier_persist() {
        let (mut g, mut a) = started();
        for n in 0..2 {
            g.write(Bytes::from_static(b"reg"));
            g.send_after_durable(out(n), &mut a);
            assert_eq!(g.after_durable(u32::from(n)), None);
            g.flush(&mut a);
        }
        a.clear();
        assert_eq!(g.persisted(PersistId(2), &mut a), Ok(alloc::vec![0, 1]));
    }

    #[test]
    fn a_checkpoint_replaces_pending_writes() {
        let (mut g, mut a) = started();
        g.write(Bytes::from_static(b"reg"));
        g.checkpoint(alloc::vec![Bytes::from_static(b"a"), Bytes::from_static(b"b")]);
        g.flush(&mut a);
        g.write(Bytes::from_static(b"reg"));
        g.flush(&mut a);
        let replace: Vec<_> = a
            .iter()
            .filter_map(|a| if let Action::Persist { replace, .. } = a { Some(*replace) } else { None })
            .collect();
        assert_eq!(replace, [true, false]);
        assert_eq!(persists(&a).iter().map(|p| p.1).collect::<Vec<_>>(), [2, 1]);
    }

    #[test]
    fn fencing_drops_waiting_work() {
        let (mut g, mut a) = started();
        g.write(Bytes::from_static(b"reg"));
        g.send_after_durable(out(6), &mut a);
        assert_eq!(g.after_durable(3), None);
        g.fence();
        g.flush(&mut a);
        assert!(a.is_empty());
    }

    proptest::proptest! {
        // Whatever the interleaving, a send made after a write leaves only once that write
        // is acknowledged, and effects run in the order they were deferred.
        #[test]
        fn sends_never_overtake_the_writes_they_follow(ops in proptest::collection::vec(0u8..5, 1..60)) {
            let (mut g, mut a) = started();
            let mut written_at: Vec<(u8, u64)> = Vec::new(); // (send tag, writes issued before it)
            let mut writes = 0u64;
            let mut issued = 1u64; // persist ids issued so far
            let mut durable_writes = 0u64;
            let mut persist_writes = alloc::vec![0u64]; // writes covered by each persist id
            let (mut tag, mut deferred, mut ran) = (0u8, 0u32, Vec::new());
            for op in ops {
                match op {
                    0 => { g.write(Bytes::new()); writes += 1; }
                    1 => { g.send_after_durable(out(tag), &mut a); written_at.push((tag, writes)); tag = tag.wrapping_add(1); }
                    2 => { if let Some(e) = g.after_durable(deferred) { ran.push(e); } deferred += 1; }
                    3 => {
                        let before = a.len();
                        g.flush(&mut a);
                        if a.len() > before { issued += 1; persist_writes.push(writes); }
                    }
                    _ => {
                        let id = issued - 1;
                        if g.acked != Some(id) {
                            ran.extend(g.persisted(PersistId(id), &mut a).unwrap());
                            durable_writes = persist_writes[id as usize];
                        }
                    }
                }
                for s in sent(&a) {
                    let (_, needed) = written_at.iter().find(|(t, _)| *t == s).copied().unwrap();
                    proptest::prop_assert!(needed <= durable_writes);
                }
                a.retain(|x| !matches!(x, Action::Send(_)));
            }
            proptest::prop_assert!(ran.windows(2).all(|w| w[0] < w[1]));
        }
    }
}

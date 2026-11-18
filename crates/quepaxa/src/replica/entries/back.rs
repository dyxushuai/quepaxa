//! Entries handed back: a batch lost its slot or its leader, the replica stepped aside, the
//! target was suspected or slow, or a snapshot released slots; and everything dropped when fenced.
//!
//! A batch in a slot may hold copies of own entries that have left it: stepping aside hands
//! its entries on and keeps the slot, so that whichever copy is decided first commits. Such
//! a copy handed back once more, or kept once the batch is out of its slot, marks its entry
//! `copied` (#187 D2, D4): it is proposed or handed on twice, and dedup absorbs the second.

use alloc::vec::Vec;

use super::{At, Entries, Left};
use crate::batch::{self, Item};
use crate::dedup::Dedup;
use crate::types::ProposalId;

impl Entries {
    /// Batch `seq` is out of its slot: the copies of own entries it keeps are no longer in a
    /// slot.
    pub(super) fn unslot(&mut self, seq: u64) {
        let Some(b) = self.batches.get_mut(&seq) else { return };
        b.slot = None;
        if b.entries.is_empty() {
            return;
        }
        self.unslotted.insert(seq);
        let (me, inc) = (self.me, self.incarnation);
        for &(o, n, id) in &b.entries {
            if o == me
                && n == inc
                && let Some(own) = self.own.get_mut(&id)
                && own.at != (At::Batched { seq })
            {
                own.copied = true;
            }
        }
    }

    /// The entries of batch `seq`, handed back to a queue, own ones moving out of it.
    fn take_back(
        &mut self,
        seq: u64,
        items: Vec<Item>,
        dedup: &Dedup,
        mut push: impl FnMut(&mut Self, Item),
    ) {
        for e in items {
            let (o, inc, id) = e.key();
            if dedup.contains(o, inc, id) {
                continue;
            }
            if self.mine(&e) {
                self.moved(id, At::Batched { seq }, At::Pending);
            }
            push(self, e);
        }
    }

    /// Batch `seq` lost its slot and is proposed again in another one.
    pub(in crate::replica) fn lost_slot_kept(&mut self, seq: u64) {
        if self.batches.contains_key(&seq) {
            self.unslot(seq);
            self.prune_due = true;
        }
    }

    /// Batch `seq` lost its slot and its entries are handed back: own ones (and, if the slot
    /// went to the target `deciding`, every one) to `pending`, those of other members to
    /// `relayed`.
    pub(in crate::replica) fn lost_slot(&mut self, seq: u64, deciding: bool, dedup: &Dedup) {
        let Some(b) = self.remove_batch(seq) else { return };
        // Our own encoding: it decodes.
        let entries = batch::decode(&b.value).unwrap_or_default();
        self.take_back(seq, entries, dedup, |s, e| {
            #[cfg(feature = "model")]
            if s.mine(&e)
                && (s.forwarded.contains_key(&e.entry.id) || s.pending.iter().any(|i| i.key() == e.key()))
            {
                s.probes.d2 += 1;
            }
            if e.entry.origin == s.me || deciding { s.push_pending(e) } else { s.push_relayed(e) }
        });
    }

    /// No longer the leader: the entries of batches not yet in a slot go to `pending`.
    pub(in crate::replica) fn lost_lead(&mut self, dedup: &Dedup) {
        let unstarted: Vec<u64> =
            self.batches.iter().filter(|(_, b)| b.slot.is_none()).map(|(seq, _)| *seq).collect();
        let mut handed = Vec::new();
        for seq in unstarted {
            if let Some(b) = self.remove_batch(seq) {
                // Our own encoding: it decodes.
                handed.push((seq, batch::decode(&b.value).unwrap_or_default()));
            }
        }
        for (seq, items) in handed {
            self.take_back(seq, items, dedup, Self::push_pending);
        }
    }

    /// Stepped aside: the entries of every local batch go to `pending` (a batch already in a
    /// slot stays there too), own ones counted as hedged, and so does everything relayed.
    pub(in crate::replica) fn step_aside(&mut self, dedup: &Dedup) {
        let mut handed = Vec::new();
        let mut unstarted = Vec::new();
        for (seq, b) in &self.batches {
            // Our own encoding: it decodes.
            handed.push((*seq, batch::decode(&b.value).unwrap_or_default()));
            if b.slot.is_none() {
                unstarted.push(*seq);
            }
        }
        for seq in unstarted {
            self.remove_batch(seq);
        }
        for (seq, items) in handed {
            self.take_back(seq, items, dedup, |s, e| {
                // Its wait on the stuck disk must not count as an ordinary commit latency.
                if e.entry.origin == s.me
                    && let Some(out) = s.own.get_mut(&e.entry.id)
                {
                    out.hedged = true;
                }
                s.push_pending(e);
            });
        }
        while let Some(e) = self.pop(true) {
            if self.mine(&e) {
                self.moved(e.entry.id, At::Relayed, At::Pending);
            }
            self.push_pending(e);
        }
    }

    /// The target is suspected: everything handed to it goes back to the front of `pending`,
    /// counted as hedged. Returns how many.
    pub(in crate::replica) fn suspect(&mut self) -> u64 {
        let mut hedged = 0;
        for (id, (e, _)) in core::mem::take(&mut self.forwarded).into_iter().rev() {
            if let Some(o) = self.own.get_mut(&id) {
                o.hedged = true;
                if matches!(o.at, At::Forwarded { .. }) {
                    o.at = At::Pending;
                }
            }
            hedged += 1;
            self.changes += usize::from(super::is_change(&e));
            self.pending.push_front(e);
        }
        hedged
    }

    /// The target left the entries `late` uncommitted too long: proposed here (counted as
    /// hedged), or, `aside`, handed to the target again. Returns how many were hedged.
    pub(in crate::replica) fn late(&mut self, late: Vec<ProposalId>, aside: bool) -> u64 {
        let mut hedged = 0;
        for id in late {
            if let Some((e, since)) = self.forwarded.remove(&id) {
                let from = At::Forwarded { since };
                if aside {
                    self.moved(id, from, At::Pending);
                    self.push_pending(e);
                    continue;
                }
                self.moved(id, from, At::Relayed);
                if let Some(o) = self.own.get_mut(&id) {
                    o.hedged = true;
                }
                hedged += 1;
                self.push_relayed(e);
            }
        }
        hedged
    }

    /// A snapshot through `next - 1` was installed with `dedup`: the own entries it contains
    /// are resolved (returned), and local batches in covered slots, still proposed or already
    /// decided there but not yet applied, are proposed again.
    pub(in crate::replica) fn installed_snapshot(&mut self, next: u64, dedup: &Dedup) -> Vec<Left> {
        let (me, inc) = (self.me, self.incarnation);
        let done: Vec<ProposalId> =
            self.own.keys().filter(|id| dedup.contains(me, inc, **id)).copied().collect();
        let left = done.into_iter().filter_map(|id| self.resolved(id)).collect();
        let released: Vec<u64> = self
            .batches
            .iter()
            .filter(|(_, b)| b.slot.is_some_and(|s| s.0 < next))
            .map(|(seq, _)| *seq)
            .collect();
        for seq in released {
            self.unslot(seq);
        }
        self.prune_due = true;
        left
    }

    /// Fenced: everything is dropped; returns the own proposals that were awaited.
    pub(in crate::replica) fn fence(&mut self) -> Vec<Left> {
        let ids: Vec<ProposalId> = self.own.keys().copied().collect();
        let left = ids.into_iter().filter_map(|id| self.leave(id)).collect();
        self.pending.clear();
        self.relayed.clear();
        self.forwarded.clear();
        self.batches.clear();
        self.unslotted.clear();
        self.foreign = 0;
        self.changes = 0;
        self.prune_due = false;
        left
    }
}

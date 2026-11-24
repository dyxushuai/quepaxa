//! Entries handed back: a batch lost its slot or its leader, the replica stepped aside, the
//! target was suspected or slow, or a snapshot released slots; and everything dropped when fenced.
//!
//! A batch in a slot may hold copies of entries that have left it: stepping aside hands its
//! entries on and keeps the slot (marked `handed`), so that whichever copy is decided first
//! commits. A slot lost by a `handed` batch hands back none of the other members' entries:
//! they went to the target when this replica stepped aside, and proposed here they would
//! wait on the stuck disk (#187 D2). Its copies of own entries are handed on again and mark
//! their entries `copied`: the lost slot shows the target decided without them, and handing
//! them on at once commits them sooner than waiting for `late` (dropping them stretched the
//! slowest follower write around a stuck leader from 253 to 342 ms). A copy kept once the
//! batch is out of its slot marks its entry `copied` too (D4). Dedup absorbs the second copy.

use alloc::vec::Vec;

use super::{At, Entries, Left};
use crate::batch::{self, Piece, Ref};
use crate::dedup::Dedup;
use crate::types::ProposalId;

impl Entries {
    /// Batch `seq` is out of its slot: the copies of own entries it keeps are no longer in a
    /// slot.
    pub(super) fn unslot(&mut self, seq: u64) {
        let Some(b) = self.batches.get_mut(&seq) else { return };
        b.slot = None;
        if b.entries.is_empty() && b.refs.is_empty() {
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

    /// The pieces of batch `seq`, handed back to a queue, own entries moving out of it;
    /// those of other members are dropped if the batch was `handed` on.
    fn take_back(
        &mut self,
        seq: u64,
        pieces: Vec<Piece>,
        handed: bool,
        dedup: &Dedup,
        mut push: impl FnMut(&mut Self, Piece),
    ) {
        for p in pieces {
            match p {
                Piece::Entry(e) => {
                    let (o, inc, id) = e.key();
                    if dedup.contains(o, inc, id) {
                        continue;
                    }
                    if self.mine(&e) {
                        self.moved(id, At::Batched { seq }, At::Pending);
                    } else if handed {
                        continue;
                    }
                    push(self, Piece::Entry(e));
                }
                Piece::Ref(r) => {
                    if handed && r.id.origin != self.me {
                        continue;
                    }
                    push(self, Piece::Ref(r));
                }
            }
        }
    }

    /// Batch `seq` lost its slot and is proposed again in another one.
    pub(in crate::replica) fn lost_slot_kept(&mut self, seq: u64) {
        if self.batches.contains_key(&seq) {
            self.unslot(seq);
            self.prune_due = true;
        }
    }

    /// Batch `seq` lost its slot and its pieces are handed back: own ones (and, if the slot
    /// went to the target `deciding`, every one) to `pending`, those of other members to
    /// `relayed`, unless the batch was handed on when this replica stepped aside: they went
    /// to the target then. Returns any references the batch held (for releasing dropped ones).
    pub(in crate::replica) fn lost_slot(&mut self, seq: u64, deciding: bool, dedup: &Dedup) -> Vec<Ref> {
        let Some(b) = self.remove_batch(seq) else { return Vec::new() };
        // Our own encoding: it decodes.
        let pieces = batch::decode_pieces(&b.value).unwrap_or_default();
        let handed = b.handed;
        let refs = b.refs;
        self.take_back(seq, pieces, handed, dedup, |s, p| {
            #[cfg(feature = "model")]
            if let Piece::Entry(e) = &p {
                if !s.mine(e) {
                    s.probes.d2_foreign += u64::from(handed);
                } else if s.forwarded.contains_key(&e.entry.id)
                    || s.pending.iter().any(|q| matches!(q, Piece::Entry(i) if i.key() == e.key()))
                {
                    s.probes.d2_own += 1;
                }
            }
            let to_pending = super::piece_origin(&p) == s.me || deciding;
            if to_pending { s.push_pending(p) } else { s.push_relayed(p) }
        });
        refs
    }

    /// No longer the leader: the pieces of batches not yet in a slot go to `pending`.
    pub(in crate::replica) fn lost_lead(&mut self, dedup: &Dedup) {
        let unstarted: Vec<u64> =
            self.batches.iter().filter(|(_, b)| b.slot.is_none()).map(|(seq, _)| *seq).collect();
        let mut handed = Vec::new();
        for seq in unstarted {
            if let Some(b) = self.remove_batch(seq) {
                // Our own encoding: it decodes.
                handed.push((seq, batch::decode_pieces(&b.value).unwrap_or_default()));
            }
        }
        for (seq, pieces) in handed {
            self.take_back(seq, pieces, false, dedup, Self::push_pending);
        }
    }

    /// Stepped aside: the pieces of every local batch go to `pending` (a batch already in a
    /// slot stays there too, `handed`), own ones counted as hedged, and so does everything
    /// relayed.
    pub(in crate::replica) fn step_aside(&mut self, dedup: &Dedup) {
        let mut handed = Vec::new();
        let mut unstarted = Vec::new();
        for (seq, b) in &mut self.batches {
            // Our own encoding: it decodes.
            handed.push((*seq, batch::decode_pieces(&b.value).unwrap_or_default()));
            if b.slot.is_none() {
                unstarted.push(*seq);
            }
            b.handed = true;
        }
        for seq in unstarted {
            self.remove_batch(seq);
        }
        for (seq, pieces) in handed {
            self.take_back(seq, pieces, false, dedup, |s, p| {
                // Its wait on the stuck disk must not count as an ordinary commit latency.
                if let Piece::Entry(e) = &p
                    && e.entry.origin == s.me
                    && let Some(out) = s.own.get_mut(&e.entry.id)
                {
                    out.hedged = true;
                }
                s.push_pending(p);
            });
        }
        while let Some(p) = self.pop(true) {
            if let Piece::Entry(e) = &p
                && self.mine(e)
            {
                self.moved(e.entry.id, At::Relayed, At::Pending);
            }
            self.push_pending(p);
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
            self.pending.push_front(Piece::Entry(e));
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
                    self.push_pending(Piece::Entry(e));
                    continue;
                }
                self.moved(id, from, At::Relayed);
                if let Some(o) = self.own.get_mut(&id) {
                    o.hedged = true;
                }
                hedged += 1;
                self.push_relayed(Piece::Entry(e));
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

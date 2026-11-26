//! Where entries are, checked at call boundaries (tests and feature `model` only; #187).
//!
//! `Entries::check` asserts E1, E2, E4 and E5, and `Replica::check_entries` adds E6;
//! `Probes` counts the combinations suspected reachable (D1-D4) without asserting them, for
//! `Inspection`.

#[cfg(test)]
use alloc::collections::VecDeque;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::{At, Entries};
#[cfg(test)]
use super::{LocalBatch, Own};
#[cfg(test)]
use crate::batch::Item;
use crate::batch::{self, Kind, Piece};
use crate::dedup::Dedup;
use crate::replica::Replica;
use crate::types::ProposalId;
#[cfg(test)]
use crate::types::Timestamp;

impl Replica {
    /// Checks where this replica's entries are, between calls: `Entries::check`, and
    ///
    /// - E6: a local batch in a slot has a proposer there, or the slot is decided and not
    ///   yet applied.
    #[doc(hidden)]
    pub fn check_entries(&self) -> Result<(), String> {
        if self.phase.fenced() {
            return Ok(());
        }
        for (seq, b) in &self.entries.batches {
            let Some(slot) = b.slot else { continue };
            let proposed = self.proposers.get(&slot).is_some_and(|a| a.batch == *seq);
            let unapplied = slot.0 >= self.emitted && self.log.contains_key(&slot);
            if !proposed && !unapplied {
                return Err(format!(
                    "E6: n{}'s batch {seq} is in slot {} with no proposer, and not decided there unapplied",
                    self.me.0, slot.0
                ));
            }
        }
        let may_hold_pending = self.proposes_directly()
            || (self.forward_flush_at().is_some()
                && !self.entries.pending_urgent(self.cfg.max_batch_bytes as usize));
        self.entries.check(may_hold_pending, &self.dedup).map_err(|e| format!("n{}: {e}", self.me.0))
    }

    /// Own entries of this incarnation both waiting to be handed on (or handed on) and in a
    /// local batch that is in a slot.
    #[cfg(feature = "model")]
    fn queued_and_slotted(&self) -> BTreeSet<ProposalId> {
        let (me, inc) = (self.me, self.incarnation);
        let e = &self.entries;
        let slotted: BTreeSet<ProposalId> = e
            .batches
            .values()
            .filter(|b| b.slot.is_some())
            .flat_map(|b| b.entries.iter().filter(|&&(o, n, _)| o == me && n == inc).map(|&(_, _, id)| id))
            .collect();
        let pending = e.pending.iter().filter_map(|p| match p {
            Piece::Entry(i) if i.entry.origin == me && i.entry.incarnation == inc => Some(i.entry.id),
            _ => None,
        });
        pending.chain(e.forwarded.keys().copied()).filter(|id| slotted.contains(id)).collect()
    }

    /// D1: counts own entries that newly sit both with the target (or on their way to it) and
    /// in a slotted batch; at the end of every `pump`.
    #[cfg(feature = "model")]
    pub(in crate::replica) fn probe_entries(&mut self) {
        let now = self.queued_and_slotted();
        let p = &mut self.entries.probes;
        p.d1 += now.difference(&p.d1_seen).count() as u64;
        p.d1_seen = now;
    }

    /// D4: entries of batches a snapshot released from their slot that another local batch
    /// still proposes in a slot.
    #[cfg(feature = "model")]
    pub(in crate::replica) fn probe_released(&mut self) {
        let e = &self.entries;
        let slotted: BTreeSet<_> =
            e.batches.values().filter(|b| b.slot.is_some()).flat_map(|b| b.entries.iter().copied()).collect();
        let both = e
            .batches
            .values()
            .filter(|b| b.slot.is_none())
            .flat_map(|b| b.entries.iter())
            .filter(|e| slotted.contains(e))
            .count();
        self.entries.probes.d4 += both as u64;
    }

    /// The suspected combinations counted so far (also in `inspect`, which costs more).
    #[cfg(feature = "model")]
    #[doc(hidden)]
    pub fn entry_probes(&self) -> crate::inspect::EntryProbes {
        let p = &self.entries.probes;
        crate::inspect::EntryProbes {
            d1: p.d1,
            d2_own: p.d2_own,
            d2_foreign: p.d2_foreign,
            d2_restarts: p.d2_restarts,
            d3: p.d3,
            d4: p.d4,
        }
    }
}

/// Where the copies of one own entry are.
#[derive(Default)]
struct Copies {
    pending: usize,
    relayed: usize,
    /// The batches holding it, once per copy, with whether each is in a slot.
    batches: Vec<(u64, bool)>,
}

impl Entries {
    /// Checks the entries between calls:
    ///
    /// - E1: every own entry is in the place its `At` names (forwarded with that time), never
    ///   a batch handed on; unless the entry is marked `copied`, that place holds one copy
    ///   and any other copy is in a batch in a slot (`verification/verus/entries.rs` proves
    ///   the same);
    /// - E2: own entries left only through a `Left`, and none is still awaited once committed;
    /// - E4: the counters match the queues and batches, and a batch `drive` would prune is
    ///   due to be pruned;
    /// - E5: a replica that does not propose its own entries directly has routed every one
    ///   of them on, unless forward coalescing holds non-urgent entries behind a scheduled
    ///   flush timer (`may_hold_pending`).
    pub(in crate::replica) fn check(&self, may_hold_pending: bool, dedup: &Dedup) -> Result<(), String> {
        self.check_own(dedup)?;
        if !may_hold_pending && !self.pending.is_empty() {
            return Err(format!("E5: hands its entries on but kept {} pending", self.pending.len()));
        }
        self.check_counters(dedup)
    }

    /// E1 and E2.
    fn check_own(&self, dedup: &Dedup) -> Result<(), String> {
        let (me, inc) = (self.me, self.incarnation);
        let (entered, left) = self.flow;
        if entered - left != self.own.len() as u64 {
            return Err(format!(
                "E2: {entered} own entries entered, {left} left, {} awaited",
                self.own.len()
            ));
        }
        let mut seen: BTreeMap<ProposalId, Copies> = BTreeMap::new();
        let mine = |p: &Piece| match p {
            Piece::Entry(i) if i.entry.origin == me && i.entry.incarnation == inc => Some(i.entry.id),
            _ => None,
        };
        for id in self.pending.iter().filter_map(mine) {
            seen.entry(id).or_default().pending += 1;
        }
        for id in self.relayed.iter().filter_map(mine) {
            seen.entry(id).or_default().relayed += 1;
        }
        for (seq, b) in &self.batches {
            for &(_, _, id) in b.entries.iter().filter(|&&(o, n, _)| o == me && n == inc) {
                seen.entry(id).or_default().batches.push((*seq, b.slot.is_some()));
            }
        }
        let none = Copies::default();
        for (id, o) in &self.own {
            if dedup.contains(me, inc, *id) {
                return Err(format!("E2: own entry {id:?} committed, still awaited"));
            }
            let c = seen.get(id).unwrap_or(&none);
            let forwarded = self.forwarded.get(id).map(|(_, t)| *t);
            let in_batch = |seq| c.batches.iter().any(|&(s, _)| s == seq);
            let held = match o.at {
                At::Pending => c.pending > 0,
                At::Relayed => c.relayed > 0,
                At::Forwarded { since } => forwarded == Some(since),
                At::Batched { seq } => in_batch(seq),
            };
            if !held {
                return Err(format!("E1: own entry {id:?} is not at {:?}", o.at));
            }
            if let At::Batched { seq } = o.at
                && self.batches.get(&seq).is_some_and(|b| b.handed)
            {
                return Err(format!("E1: own entry {id:?} is at batch {seq}, which was handed on"));
            }
            // Copies in the entry's own batch count whether or not it is in a slot.
            let own_batch = |s: u64| matches!(o.at, At::Batched { seq } if seq == s);
            let batched = c.batches.iter().filter(|&&(s, slotted)| !slotted || own_batch(s)).count();
            let copies = c.pending + c.relayed + usize::from(forwarded.is_some()) + batched - 1;
            if copies > 0 && !o.copied {
                return Err(format!(
                    "E1: own entry {id:?} at {:?} has {copies} copies outside it and the slotted batches",
                    o.at
                ));
            }
        }
        Ok(())
    }

    /// E4: the counters, recomputed.
    fn check_counters(&self, dedup: &Dedup) -> Result<(), String> {
        let unslotted: BTreeSet<u64> = self
            .batches
            .iter()
            .filter(|(_, b)| b.slot.is_none() && (!b.entries.is_empty() || !b.refs.is_empty()))
            .map(|(seq, _)| *seq)
            .collect();
        let foreign = self.relayed.iter().filter(|p| super::piece_origin(p) != self.me).count()
            + self.batches.values().map(|b| self.foreign_in(b)).sum::<usize>();
        let changes = self.own.values().filter(|o| o.kind == Kind::Reconfigure).count()
            + self.pending.iter().chain(&self.relayed).filter(|p| super::piece_is_change(p)).count()
            + self.batches.values().map(|b| batch::changes(&b.value).len()).sum::<usize>();
        if (&unslotted, foreign, changes) != (&self.unslotted, self.foreign, self.changes) {
            return Err(format!(
                "E4: counts unslotted {:?}, foreign {}, changes {}; holds {unslotted:?}, {foreign}, {changes}",
                self.unslotted, self.foreign, self.changes
            ));
        }
        let dead = self.batches.iter().find(|(_, b)| {
            b.slot.is_none()
                && b.refs.is_empty()
                && !b.entries.iter().any(|&(o, i, id)| !dedup.contains(o, i, id))
        });
        if let Some((seq, _)) = dead
            && !self.prune_due
        {
            return Err(format!("E4: batch {seq} has nothing left to commit, and no prune is due"));
        }
        Ok(())
    }
}

/// Counts of the suspected combinations (feature `model`); never part of the replica's
/// fingerprint (`Entries` hashes without them), so exploring and storm detection see the
/// same states as without them.
#[cfg(feature = "model")]
#[derive(Clone, Default)]
pub(in crate::replica) struct Probes {
    pub(in crate::replica) d1: u64,
    d1_seen: BTreeSet<ProposalId>,
    pub(in crate::replica) d2_own: u64,
    pub(in crate::replica) d2_foreign: u64,
    pub(in crate::replica) d2_restarts: u64,
    pub(in crate::replica) d3: u64,
    d4: u64,
}

/// What tests poke at in place of the fields.
#[cfg(test)]
impl Entries {
    pub(in crate::replica) fn pending(&self) -> &VecDeque<Piece> {
        &self.pending
    }

    pub(in crate::replica) fn relayed(&self) -> &VecDeque<Piece> {
        &self.relayed
    }

    pub(in crate::replica) fn forwarded(&self) -> &BTreeMap<ProposalId, (Item, Timestamp)> {
        &self.forwarded
    }

    pub(in crate::replica) fn batches(&self) -> &BTreeMap<u64, LocalBatch> {
        &self.batches
    }

    pub(in crate::replica) fn own(&self) -> &BTreeMap<ProposalId, Own> {
        &self.own
    }

    pub(in crate::replica) fn queue_pending(&mut self, i: Item) {
        self.push_pending(Piece::Entry(i));
    }

    pub(in crate::replica) fn queue_relayed(&mut self, i: Item) {
        self.push_relayed(Piece::Entry(i));
    }

    /// An own entry with no place: the replica's checks reject it.
    pub(in crate::replica) fn insert_own(&mut self, id: ProposalId, since: Timestamp, kind: Kind) {
        self.changes += usize::from(kind == Kind::Reconfigure);
        self.flow.0 += 1;
        self.own.insert(id, Own { since, kind, hedged: false, at: At::Pending, copied: false });
    }
}

//! Where entries are, checked at call boundaries (tests and feature `model` only; #187).
//!
//! `check_entries` asserts what holds of today's queues; `Probes` counts the combinations
//! suspected reachable (D1-D4) without asserting them, for `Inspection`.

use alloc::collections::BTreeSet;
#[cfg(test)]
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::String;

#[cfg(test)]
use super::{Entries, LocalBatch, Own};
#[cfg(test)]
use crate::batch::Item;
use crate::batch::{self, Kind};
use crate::replica::Replica;
use crate::types::ProposalId;
#[cfg(test)]
use crate::types::Timestamp;

impl Replica {
    /// Checks where this replica's entries are, between calls:
    ///
    /// - I2: every own proposal awaiting commit is queued, handed to the target, or in a
    ///   local batch;
    /// - E6: a local batch in a slot has a proposer there, or the slot is decided and not
    ///   yet applied;
    /// - I1/E5: a replica that does not propose its own entries itself has handed every one
    ///   of them on;
    /// - E4: the counters match the queues and batches, and a batch `drive` would prune is
    ///   due to be pruned.
    #[doc(hidden)]
    pub fn check_entries(&self) -> Result<(), String> {
        if self.fenced {
            return Ok(());
        }
        let (me, inc) = (self.me, self.incarnation);
        let mine = |o, i| o == me && i == inc;
        let e = &self.entries;
        for id in e.own.keys() {
            let queued = e.pending.iter().chain(&e.relayed).any(|i| {
                let (o, n, e) = i.key();
                mine(o, n) && e == *id
            });
            let batched =
                e.batches.values().any(|b| b.entries.iter().any(|&(o, n, e)| mine(o, n) && e == *id));
            if !queued && !batched && !e.forwarded.contains_key(id) {
                return Err(format!("I2: n{} lost track of its own entry {id:?}", me.0));
            }
        }
        for (seq, b) in &e.batches {
            let Some(slot) = b.slot else { continue };
            let proposed = self.proposers.get(&slot).is_some_and(|a| a.batch == *seq);
            let unapplied = slot.0 >= self.emitted && self.log.contains_key(&slot);
            if !proposed && !unapplied {
                return Err(format!(
                    "E6: n{}'s batch {seq} is in slot {} with no proposer, and not decided there unapplied",
                    me.0, slot.0
                ));
            }
        }
        if !self.proposes_directly() && !e.pending.is_empty() {
            return Err(format!(
                "I1/E5: n{} hands its entries to n{} but kept {} pending",
                me.0,
                self.target().0,
                e.pending.len()
            ));
        }
        self.check_counters()
    }

    /// E4: the counters of `entries.rs`, recomputed.
    fn check_counters(&self) -> Result<(), String> {
        let e = &self.entries;
        let unslotted: BTreeSet<u64> = e
            .batches
            .iter()
            .filter(|(_, b)| b.slot.is_none() && !b.entries.is_empty())
            .map(|(seq, _)| *seq)
            .collect();
        let foreign = e.relayed.iter().filter(|i| i.entry.origin != self.me).count()
            + e.batches.values().flat_map(|b| &b.entries).filter(|&&(o, _, _)| o != self.me).count();
        let changes = e.own.values().filter(|o| o.kind == Kind::Reconfigure).count()
            + e.pending.iter().chain(&e.relayed).filter(|i| i.kind == Kind::Reconfigure).count()
            + e.batches.values().map(|b| batch::changes(&b.value).len()).sum::<usize>();
        if (&unslotted, foreign, changes) != (&e.unslotted, e.foreign, e.changes) {
            return Err(format!(
                "E4: n{} counts unslotted {:?}, foreign {}, changes {}; holds {unslotted:?}, {foreign}, {changes}",
                self.me.0, e.unslotted, e.foreign, e.changes
            ));
        }
        let dead = e.batches.iter().find(|(_, b)| {
            b.slot.is_none() && !b.entries.iter().any(|&(o, i, id)| !self.dedup.contains(o, i, id))
        });
        if let Some((seq, _)) = dead
            && !e.prune_due
        {
            return Err(format!(
                "E4: n{}'s batch {seq} has nothing left to commit, and no prune is due",
                self.me.0
            ));
        }
        Ok(())
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
        let pending = e.pending.iter().filter(|i| i.entry.origin == me && i.entry.incarnation == inc);
        pending
            .map(|i| i.entry.id)
            .chain(e.forwarded.keys().copied())
            .filter(|id| slotted.contains(id))
            .collect()
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
        crate::inspect::EntryProbes { d1: p.d1, d2: p.d2, d2_restarts: p.d2_restarts, d3: p.d3, d4: p.d4 }
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
    pub(in crate::replica) d2: u64,
    pub(in crate::replica) d2_restarts: u64,
    pub(in crate::replica) d3: u64,
    d4: u64,
}

/// What tests poke at in place of the fields.
#[cfg(test)]
impl Entries {
    pub(in crate::replica) fn pending(&self) -> &VecDeque<Item> {
        &self.pending
    }

    pub(in crate::replica) fn relayed(&self) -> &VecDeque<Item> {
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
        self.push_pending(i);
    }

    pub(in crate::replica) fn queue_relayed(&mut self, i: Item) {
        self.push_relayed(i);
    }

    pub(in crate::replica) fn insert_own(&mut self, id: ProposalId, since: Timestamp, kind: Kind) {
        self.changes += usize::from(kind == Kind::Reconfigure);
        self.own.insert(id, Own { since, kind, hedged: false });
    }
}

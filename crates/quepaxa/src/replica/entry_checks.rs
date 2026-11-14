//! Where entries are, checked at call boundaries (tests and feature `model` only; #187).
//!
//! `check_entries` asserts what holds of today's queues; `Probes` counts the combinations
//! suspected reachable (D1-D4) without asserting them, for `Inspection`.

#[cfg(feature = "model")]
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;

use super::Replica;
#[cfg(feature = "model")]
use crate::types::ProposalId;

impl Replica {
    /// Checks where this replica's entries are, between calls:
    ///
    /// - I2: every own proposal awaiting commit is queued, handed to the target, or in a
    ///   local batch;
    /// - E6: a local batch in a slot has a proposer there, or the slot is decided and not
    ///   yet applied;
    /// - I1/E5: a replica that does not propose its own entries itself has handed every one
    ///   of them on.
    #[doc(hidden)]
    pub fn check_entries(&self) -> Result<(), String> {
        if self.fenced {
            return Ok(());
        }
        let (me, inc) = (self.me, self.incarnation);
        let mine = |o, i| o == me && i == inc;
        for id in self.outstanding.keys() {
            let queued = self.pending.iter().chain(&self.relayed).any(|i| {
                let (o, n, e) = i.key();
                mine(o, n) && e == *id
            });
            let batched =
                self.batches.values().any(|b| b.entries.iter().any(|&(o, n, e)| mine(o, n) && e == *id));
            if !queued && !batched && !self.forwarded.contains_key(id) {
                return Err(format!("I2: n{} lost track of its own entry {id:?}", me.0));
            }
        }
        for (seq, b) in &self.batches {
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
        if !self.proposes_directly() && !self.pending.is_empty() {
            return Err(format!(
                "I1/E5: n{} hands its entries to n{} but kept {} pending",
                me.0,
                self.target().0,
                self.pending.len()
            ));
        }
        Ok(())
    }

    /// Own entries of this incarnation both waiting to be handed on (or handed on) and in a
    /// local batch that is in a slot.
    #[cfg(feature = "model")]
    fn queued_and_slotted(&self) -> BTreeSet<ProposalId> {
        let (me, inc) = (self.me, self.incarnation);
        let slotted: BTreeSet<ProposalId> = self
            .batches
            .values()
            .filter(|b| b.slot.is_some())
            .flat_map(|b| b.entries.iter().filter(|&&(o, n, _)| o == me && n == inc).map(|&(_, _, id)| id))
            .collect();
        let pending = self.pending.iter().filter(|i| i.entry.origin == me && i.entry.incarnation == inc);
        pending
            .map(|i| i.entry.id)
            .chain(self.forwarded.keys().copied())
            .filter(|id| slotted.contains(id))
            .collect()
    }

    /// D1: counts own entries that newly sit both with the target (or on their way to it) and
    /// in a slotted batch; at the end of every `pump`.
    #[cfg(feature = "model")]
    pub(super) fn probe_entries(&mut self) {
        let now = self.queued_and_slotted();
        self.probes.d1 += now.difference(&self.probes.d1_seen).count() as u64;
        self.probes.d1_seen = now;
    }

    /// D4: entries of batches a snapshot released from their slot that another local batch
    /// still proposes in a slot.
    #[cfg(feature = "model")]
    pub(super) fn probe_released(&mut self) {
        let slotted: BTreeSet<_> = self
            .batches
            .values()
            .filter(|b| b.slot.is_some())
            .flat_map(|b| b.entries.iter().copied())
            .collect();
        let both = self
            .batches
            .values()
            .filter(|b| b.slot.is_none())
            .flat_map(|b| b.entries.iter())
            .filter(|e| slotted.contains(e))
            .count();
        self.probes.d4 += both as u64;
    }

    /// The suspected combinations counted so far (also in `inspect`, which costs more).
    #[cfg(feature = "model")]
    #[doc(hidden)]
    pub fn entry_probes(&self) -> crate::inspect::EntryProbes {
        let p = &self.probes;
        crate::inspect::EntryProbes { d1: p.d1, d2: p.d2, d2_restarts: p.d2_restarts, d3: p.d3, d4: p.d4 }
    }
}

/// Counts of the suspected combinations (feature `model`); never part of the replica's
/// fingerprint, so exploring and storm detection see the same states as without them.
#[cfg(feature = "model")]
#[derive(Clone, Default)]
pub(super) struct Probes {
    pub(super) d1: u64,
    d1_seen: BTreeSet<ProposalId>,
    pub(super) d2: u64,
    pub(super) d2_restarts: u64,
    pub(super) d3: u64,
    d4: u64,
}

#[cfg(feature = "model")]
impl core::hash::Hash for Probes {
    fn hash<H: core::hash::Hasher>(&self, _: &mut H) {}
}

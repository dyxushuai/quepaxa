//! Where this replica's entries wait until they commit (#187): its own, from `propose` until
//! they are resolved, and those other members handed it.
//!
//! - `pending`: entries to batch if this replica proposes them itself, else to forward to
//!   its target (`watch.rs`).
//! - `relayed`: entries this replica proposes itself whatever it leads: handed to it as the
//!   deputy of a leader another follower suspects, or its own the leader left uncommitted
//!   for δ (`watch.rs`).
//! - `forwarded`: own entries handed to the target, with when.
//! - `batches`: batches this replica proposes, each in at most one slot at a time.
//! - `own`: own proposals awaiting commit.
//!
//! The replica decides who proposes and when; the methods here, named after what triggers
//! them, move the entries. Counters kept alongside answer in O(1) what the replica asks on
//! every input: whether a batch waits for a slot (`unslotted`), whether another member's
//! entries are here (`foreign`), whether a membership change is on its way (`changes`), and
//! whether a batch may have become dead weight (`prune_due`).

#[cfg(any(test, feature = "model"))]
mod checks;

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;
use bytes::Bytes;

use crate::batch::{self, Item, Kind};
use crate::dedup::Dedup;
use crate::proposal::ValueKey;
use crate::types::{NodeId, ProposalId, Slot, Timestamp};

/// A batch this replica proposes, in at most one slot at a time until it is decided.
#[cfg_attr(feature = "model", derive(Clone, Hash))]
pub(super) struct LocalBatch {
    pub(super) key: ValueKey,
    pub(super) value: Bytes,
    pub(super) entries: Vec<(NodeId, u64, ProposalId)>,
    pub(super) slot: Option<Slot>,
    /// Membership changes among the entries.
    changes: usize,
}

/// An own proposal awaiting commit.
#[derive(Clone, Copy)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct Own {
    /// When it was made.
    pub(super) since: Timestamp,
    pub(super) kind: Kind,
    /// Whether it was hedged: its commit latency is not an ordinary one.
    pub(super) hedged: bool,
}

#[cfg_attr(feature = "model", derive(Clone))]
pub(super) struct Entries {
    me: NodeId,
    incarnation: u64,
    pending: VecDeque<Item>,
    relayed: VecDeque<Item>,
    forwarded: BTreeMap<ProposalId, (Item, Timestamp)>,
    batches: BTreeMap<u64, LocalBatch>,
    own: BTreeMap<ProposalId, Own>,
    next_batch: u64,
    /// Batches with entries and no slot, which `drive` proposes before cutting a new one.
    unslotted: BTreeSet<u64>,
    /// Entries of other members in `relayed` and in batches.
    foreign: usize,
    /// Membership changes in `own`, the queues and the batches.
    changes: usize,
    /// A batch without a slot may have no entry left to commit: one lost its slot, or the
    /// dedup state grew or was replaced while one waited.
    prune_due: bool,
    /// Suspected entry combinations seen, for verification only (`checks.rs`).
    #[cfg(feature = "model")]
    pub(super) probes: checks::Probes,
}

/// The counters follow from the rest, or from history (`prune_due`), and the probes only
/// count: none of them is part of the replica's fingerprint, so exploring sees the same states.
#[cfg(feature = "model")]
impl core::hash::Hash for Entries {
    fn hash<H: core::hash::Hasher>(&self, h: &mut H) {
        (&self.pending, &self.relayed, &self.forwarded, &self.batches, &self.own, self.next_batch).hash(h);
    }
}

fn is_change(i: &Item) -> bool {
    i.kind == Kind::Reconfigure
}

impl Entries {
    pub(super) fn new(me: NodeId, incarnation: u64) -> Self {
        Self {
            me,
            incarnation,
            pending: VecDeque::new(),
            relayed: VecDeque::new(),
            forwarded: BTreeMap::new(),
            batches: BTreeMap::new(),
            own: BTreeMap::new(),
            next_batch: 0,
            unslotted: BTreeSet::new(),
            foreign: 0,
            changes: 0,
            prune_due: false,
            #[cfg(feature = "model")]
            probes: checks::Probes::default(),
        }
    }

    fn foreign_in(&self, ids: &[(NodeId, u64, ProposalId)]) -> usize {
        ids.iter().filter(|&&(o, _, _)| o != self.me).count()
    }

    // ---- queues and batches, with their counters ----

    fn push_pending(&mut self, i: Item) {
        self.changes += usize::from(is_change(&i));
        self.pending.push_back(i);
    }

    fn push_relayed(&mut self, i: Item) {
        self.changes += usize::from(is_change(&i));
        self.foreign += usize::from(i.entry.origin != self.me);
        self.relayed.push_back(i);
    }

    fn pop(&mut self, relayed: bool) -> Option<Item> {
        let i = if relayed { self.relayed.pop_front() } else { self.pending.pop_front() }?;
        self.changes -= usize::from(is_change(&i));
        self.foreign -= usize::from(relayed && i.entry.origin != self.me);
        Some(i)
    }

    fn insert_batch(&mut self, seq: u64, b: LocalBatch) {
        self.foreign += self.foreign_in(&b.entries);
        self.changes += b.changes;
        if b.slot.is_none() && !b.entries.is_empty() {
            self.unslotted.insert(seq);
        }
        self.batches.insert(seq, b);
    }

    fn remove_batch(&mut self, seq: u64) -> Option<LocalBatch> {
        let b = self.batches.remove(&seq)?;
        self.foreign -= self.foreign_in(&b.entries);
        self.changes -= b.changes;
        self.unslotted.remove(&seq);
        Some(b)
    }

    // ---- what the replica asks ----

    /// Own proposals awaiting commit.
    pub(super) fn own_len(&self) -> usize {
        self.own.len()
    }

    /// Whether an own membership change awaits commit.
    pub(super) fn proposing_change(&self) -> bool {
        self.own.values().any(|o| o.kind == Kind::Reconfigure)
    }

    /// Whether a membership change is anywhere here: own, queued, or in a batch.
    pub(super) fn any_change(&self) -> bool {
        self.changes > 0
    }

    /// Whether this replica holds entries other members handed it (as their deputy).
    pub(super) fn deputy(&self) -> bool {
        self.foreign > 0
    }

    /// The first batch with entries waiting for a slot.
    pub(super) fn waiting(&self) -> Option<u64> {
        self.unslotted.first().copied()
    }

    /// Whether `cut` has entries to take.
    pub(super) fn to_cut(&self, direct: bool) -> bool {
        !self.relayed.is_empty() || (direct && !self.pending.is_empty())
    }

    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Own entries handed to the target, with when.
    pub(super) fn forwarded_since(&self) -> impl Iterator<Item = (ProposalId, Timestamp)> + '_ {
        self.forwarded.iter().map(|(id, (_, at))| (*id, *at))
    }

    /// The key of the value batch `seq` proposes.
    pub(super) fn batch_key(&self, seq: u64) -> Option<ValueKey> {
        self.batches.get(&seq).map(|b| b.key)
    }

    #[cfg(feature = "model")]
    pub(super) fn batch_count(&self) -> usize {
        self.batches.len()
    }

    // ---- transitions ----

    /// `propose` and the like queued an entry of this replica's.
    pub(super) fn submit(&mut self, now: Timestamp, item: Item) {
        self.own.insert(item.entry.id, Own { since: now, kind: item.kind, hedged: false });
        if is_change(&item) {
            self.changes += 1;
        }
        self.push_pending(item);
    }

    /// A follower handed entries to this replica: into `pending` (to hand on, or as the
    /// leader) or `relayed` (as a deputy), unless that queue would exceed `cap`.
    pub(super) fn received(&mut self, entries: Vec<Item>, to_pending: bool, cap: usize) -> bool {
        #[cfg(feature = "model")]
        {
            let own = entries
                .iter()
                .filter(|i| i.entry.origin == self.me && i.entry.incarnation == self.incarnation);
            self.probes.d3 += own.count() as u64;
        }
        let queued = if to_pending { self.pending.len() } else { self.relayed.len() };
        if queued + entries.len() > cap {
            return false; // the origin hedges
        }
        for e in entries {
            if to_pending { self.push_pending(e) } else { self.push_relayed(e) }
        }
        true
    }

    /// Hands everything pending to the target: own entries are tracked from `now`.
    pub(super) fn forward(&mut self, now: Timestamp) -> Vec<Item> {
        let entries: Vec<Item> = self.pending.drain(..).collect();
        self.changes -= entries.iter().filter(|i| is_change(i)).count();
        let (me, inc) = (self.me, self.incarnation);
        for i in entries.iter().filter(|i| i.entry.origin == me && i.entry.incarnation == inc) {
            let _earlier = self.forwarded.insert(i.entry.id, (i.clone(), now));
            #[cfg(feature = "model")]
            if _earlier.is_some_and(|(_, at)| at < now) {
                self.probes.d2_restarts += 1;
            }
        }
        entries
    }

    /// Batches whose entries all committed elsewhere are dead weight.
    pub(super) fn prune(&mut self, dedup: &Dedup) {
        if !self.prune_due {
            return;
        }
        self.prune_due = false;
        let dead: Vec<u64> = self
            .batches
            .iter()
            .filter(|(_, b)| {
                b.slot.is_none() && !b.entries.iter().any(|&(o, i, id)| !dedup.contains(o, i, id))
            })
            .map(|(seq, _)| *seq)
            .collect();
        for seq in dead {
            self.remove_batch(seq);
        }
    }

    /// Cuts the next batch: relayed entries always, own pending ones only when proposing them
    /// directly (`lead`).
    pub(super) fn cut(&mut self, lead: bool, max_bytes: usize, dedup: &Dedup) -> Option<u64> {
        let mut entries = batch::Builder::new(max_bytes);
        loop {
            let relayed = if !self.relayed.is_empty() {
                true
            } else if lead && !self.pending.is_empty() {
                false
            } else {
                break;
            };
            let queue = if relayed { &self.relayed } else { &self.pending };
            let Some(e) = queue.front() else { break };
            if !entries.fits(e) {
                break;
            }
            let Some(e) = self.pop(relayed) else { break };
            let (o, inc, id) = e.key();
            if dedup.contains(o, inc, id) {
                continue;
            }
            entries.push(e);
        }
        (!entries.is_empty()).then(|| self.new_batch(entries.take()))
    }

    /// A new local batch of `entries`, not yet in a slot.
    pub(super) fn new_batch(&mut self, entries: Vec<Item>) -> u64 {
        let seq = self.next_batch;
        self.next_batch += 1;
        let key = ValueKey { origin: self.me, incarnation: self.incarnation, seq };
        let ids = entries.iter().map(Item::key).collect();
        let changes = entries.iter().filter(|i| is_change(i)).count();
        let b = LocalBatch { key, value: batch::encode(&entries), entries: ids, slot: None, changes };
        self.insert_batch(seq, b);
        seq
    }

    /// Batch `seq` is proposed in `slot`: the key and value to propose.
    pub(super) fn start(&mut self, seq: u64, slot: Slot) -> Option<(ValueKey, Bytes)> {
        let b = self.batches.get_mut(&seq)?;
        b.slot = Some(slot);
        self.unslotted.remove(&seq);
        Some((b.key, b.value.clone()))
    }

    /// Batch `seq` lost its slot and is proposed again in another one.
    pub(super) fn lost_slot_kept(&mut self, seq: u64) {
        if let Some(b) = self.batches.get_mut(&seq) {
            b.slot = None;
            if !b.entries.is_empty() {
                self.unslotted.insert(seq);
            }
            self.prune_due = true;
        }
    }

    /// Batch `seq` lost its slot and its entries are handed back: own ones (and, if the slot
    /// went to the target `deciding`, every one) to `pending`, those of other members to
    /// `relayed`.
    pub(super) fn lost_slot(&mut self, seq: u64, deciding: bool, dedup: &Dedup) {
        let Some(b) = self.remove_batch(seq) else { return };
        // Our own encoding: it decodes.
        let entries = batch::decode(&b.value).unwrap_or_default();
        for e in entries {
            let (o, inc, id) = e.key();
            if dedup.contains(o, inc, id) {
                continue;
            }
            #[cfg(feature = "model")]
            if o == self.me
                && inc == self.incarnation
                && (self.forwarded.contains_key(&id) || self.pending.iter().any(|i| i.key() == (o, inc, id)))
            {
                self.probes.d2 += 1;
            }
            if o == self.me || deciding { self.push_pending(e) } else { self.push_relayed(e) }
        }
    }

    /// No longer the leader: the entries of batches not yet in a slot go to `pending`.
    pub(super) fn lost_lead(&mut self, dedup: &Dedup) {
        let unstarted: Vec<u64> =
            self.batches.iter().filter(|(_, b)| b.slot.is_none()).map(|(seq, _)| *seq).collect();
        let mut handed = Vec::new();
        for seq in unstarted {
            if let Some(b) = self.remove_batch(seq) {
                // Our own encoding: it decodes.
                handed.extend(batch::decode(&b.value).unwrap_or_default());
            }
        }
        for e in handed {
            let (o, inc, id) = e.key();
            if !dedup.contains(o, inc, id) {
                self.push_pending(e);
            }
        }
    }

    /// Stepped aside: the entries of every local batch go to `pending` (a batch already in a
    /// slot stays there too), own ones counted as hedged, and so does everything relayed.
    pub(super) fn step_aside(&mut self, dedup: &Dedup) {
        let mut handed = Vec::new();
        let mut unstarted = Vec::new();
        for (seq, b) in &self.batches {
            // Our own encoding: it decodes.
            handed.extend(batch::decode(&b.value).unwrap_or_default());
            if b.slot.is_none() {
                unstarted.push(*seq);
            }
        }
        for seq in unstarted {
            self.remove_batch(seq);
        }
        for e in handed {
            let (o, inc, id) = e.key();
            if dedup.contains(o, inc, id) {
                continue;
            }
            // Its wait on the stuck disk must not count as an ordinary commit latency.
            if o == self.me
                && let Some(out) = self.own.get_mut(&id)
            {
                out.hedged = true;
            }
            self.push_pending(e);
        }
        while let Some(e) = self.pop(true) {
            self.push_pending(e);
        }
    }

    /// The target is suspected: everything handed to it goes back to the front of `pending`,
    /// counted as hedged. Returns how many.
    pub(super) fn suspect(&mut self) -> u64 {
        let mut hedged = 0;
        for (id, (e, _)) in core::mem::take(&mut self.forwarded).into_iter().rev() {
            if let Some(o) = self.own.get_mut(&id) {
                o.hedged = true;
            }
            hedged += 1;
            self.changes += usize::from(is_change(&e));
            self.pending.push_front(e);
        }
        hedged
    }

    /// The target left the entries `late` uncommitted too long: proposed here (counted as
    /// hedged), or, `aside`, handed to the target again. Returns how many were hedged.
    pub(super) fn late(&mut self, late: Vec<ProposalId>, aside: bool) -> u64 {
        let mut hedged = 0;
        for id in late {
            if let Some((e, _)) = self.forwarded.remove(&id) {
                if aside {
                    self.push_pending(e);
                    continue;
                }
                if let Some(o) = self.own.get_mut(&id) {
                    o.hedged = true;
                }
                hedged += 1;
                self.push_relayed(e);
            }
        }
        hedged
    }

    /// An own entry is resolved (committed, superseded, or internal): no longer handed to
    /// anyone nor awaited.
    pub(super) fn resolved(&mut self, id: ProposalId) -> Option<Own> {
        self.forwarded.remove(&id);
        let own = self.own.remove(&id)?;
        self.changes -= usize::from(own.kind == Kind::Reconfigure);
        Some(own)
    }

    /// Own batch `seq` was decided and applied.
    pub(super) fn applied(&mut self, seq: u64) {
        self.remove_batch(seq);
    }

    /// Entries committed: a batch waiting for a slot may hold nothing else.
    pub(super) fn dedup_grew(&mut self) {
        self.prune_due |= !self.unslotted.is_empty();
    }

    /// A snapshot through `next - 1` was installed with `dedup`: the own entries it contains
    /// are resolved (returned, with their kinds), and local batches in covered slots, still
    /// proposed or already decided there but not yet applied, are proposed again.
    pub(super) fn installed_snapshot(&mut self, next: u64, dedup: &Dedup) -> Vec<(ProposalId, Kind)> {
        let (me, inc) = (self.me, self.incarnation);
        let done: Vec<_> = self
            .own
            .iter()
            .filter(|(id, _)| dedup.contains(me, inc, **id))
            .map(|(id, o)| (*id, o.kind))
            .collect();
        for &(id, _) in &done {
            self.resolved(id);
        }
        for (seq, b) in &mut self.batches {
            if b.slot.is_some_and(|s| s.0 < next) {
                b.slot = None;
                if !b.entries.is_empty() {
                    self.unslotted.insert(*seq);
                }
            }
        }
        self.prune_due = true;
        done
    }

    /// Fenced: everything is dropped; returns the own proposals that were awaited.
    pub(super) fn fence(&mut self) -> alloc::collections::btree_map::IntoKeys<ProposalId, Own> {
        let own = core::mem::take(&mut self.own).into_keys();
        self.pending.clear();
        self.relayed.clear();
        self.forwarded.clear();
        self.batches.clear();
        self.unslotted.clear();
        self.foreign = 0;
        self.changes = 0;
        self.prune_due = false;
        own
    }
}

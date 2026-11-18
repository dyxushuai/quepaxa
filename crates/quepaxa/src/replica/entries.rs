//! Where this replica's entries wait until they commit (#187): its own, from `propose` until
//! they are resolved, and those other members handed it.
//!
//! - `pending`: entries to batch if this replica proposes them itself, else to hand to its
//!   target (`route`).
//! - `relayed`: entries this replica proposes itself whatever it leads: handed to it as the
//!   deputy of a leader another follower suspects, or its own the leader left uncommitted
//!   for δ (`watch.rs`).
//! - `forwarded`: own entries handed to the target, with when.
//! - `batches`: batches this replica proposes, each in at most one slot at a time.
//! - `own`: own proposals awaiting commit, each with the place `At` that holds it.
//!
//! The replica decides who proposes and when; the methods here, named after what triggers
//! them, move the entries, and an own entry's `At` with it. An own entry leaves `own` only
//! through a returned `Left`. Copies of an own entry outside its place are allowed in a
//! batch already in a slot (stepping aside keeps those slots: whichever copy is decided
//! first commits); a copy anywhere else marks the entry `copied` (`entries/back.rs`).
//! Counters kept alongside answer in O(1) what the replica asks on every input: whether a
//! batch waits for a slot (`unslotted`), whether another member's entries are here
//! (`foreign`), whether a membership change is on its way (`changes`), and whether a batch
//! may have become dead weight (`prune_due`).

mod back;
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
    /// Its entries were handed on when the replica stepped aside: a lost slot hands back
    /// none of the other members' (#187 D2).
    handed: bool,
}

/// The place that holds an own entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum At {
    Pending,
    Relayed,
    /// Handed to the target at `since`.
    Forwarded {
        since: Timestamp,
    },
    /// In local batch `seq`.
    Batched {
        seq: u64,
    },
}

impl At {
    /// The same place, whenever a forwarded entry was handed on.
    fn same(self, other: At) -> bool {
        match (self, other) {
            (At::Forwarded { .. }, At::Forwarded { .. }) => true,
            _ => self == other,
        }
    }
}

/// An own proposal awaiting commit.
#[derive(Clone, Copy)]
pub(super) struct Own {
    /// When it was made.
    pub(super) since: Timestamp,
    pub(super) kind: Kind,
    /// Whether it was hedged: its commit latency is not an ordinary one.
    pub(super) hedged: bool,
    pub(super) at: At,
    /// A copy of it went outside `at` and the batches in a slot: re-queued from a batch it
    /// had already left (#187 D2), handed back by a deputy (D3), or kept by a batch out of
    /// its slot (D4).
    copied: bool,
}

/// `at` and `copied` follow from the queues, or from history: not part of the fingerprint.
#[cfg(feature = "model")]
impl core::hash::Hash for Own {
    fn hash<H: core::hash::Hasher>(&self, h: &mut H) {
        (self.since, self.kind, self.hedged).hash(h);
    }
}

/// An own entry no longer awaited (committed, superseded, internal, resolved by a snapshot,
/// or fenced): the only way one leaves `own` (E2).
#[must_use]
pub(super) struct Left {
    pub(super) id: ProposalId,
    pub(super) own: Own,
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
    /// Own entries that entered and left, for E2 (`checks.rs`).
    #[cfg(any(test, feature = "model"))]
    flow: (u64, u64),
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
            #[cfg(any(test, feature = "model"))]
            flow: (0, 0),
            #[cfg(feature = "model")]
            probes: checks::Probes::default(),
        }
    }

    fn foreign_in(&self, ids: &[(NodeId, u64, ProposalId)]) -> usize {
        ids.iter().filter(|&&(o, _, _)| o != self.me).count()
    }

    /// An entry of this replica's current incarnation.
    fn mine(&self, i: &Item) -> bool {
        i.entry.origin == self.me && i.entry.incarnation == self.incarnation
    }

    /// Own entry `id` was taken from `from` to `to`: the entry moves if `from` holds it,
    /// else what moved is a copy.
    fn moved(&mut self, id: ProposalId, from: At, to: At) {
        if let Some(o) = self.own.get_mut(&id) {
            if o.at.same(from) { o.at = to } else { o.copied = true }
        }
    }

    /// Own entry `id` leaves `own`.
    fn leave(&mut self, id: ProposalId) -> Option<Left> {
        let own = self.own.remove(&id)?;
        self.changes -= usize::from(own.kind == Kind::Reconfigure);
        #[cfg(any(test, feature = "model"))]
        {
            self.flow.1 += 1;
        }
        Some(Left { id, own })
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

    /// Whether there is something to propose: a batch waiting for a slot, or entries `cut`
    /// takes (own pending ones only when proposing them directly).
    pub(super) fn proposable(&self, direct: bool) -> bool {
        !self.unslotted.is_empty() || !self.relayed.is_empty() || (direct && !self.pending.is_empty())
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
        let own = Own { since: now, kind: item.kind, hedged: false, at: At::Pending, copied: false };
        self.own.insert(item.entry.id, own);
        #[cfg(any(test, feature = "model"))]
        {
            self.flow.0 += 1;
        }
        if is_change(&item) {
            self.changes += 1;
        }
        self.push_pending(item);
    }

    /// A follower handed entries to this replica: into `pending` (to hand on, or as the
    /// leader) or `relayed` (as a deputy), unless that queue would exceed `cap`. An own entry
    /// among them is a copy.
    pub(super) fn received(&mut self, entries: Vec<Item>, to_pending: bool, cap: usize) -> bool {
        #[cfg(feature = "model")]
        {
            let own = entries.iter().filter(|i| self.mine(i));
            self.probes.d3 += own.count() as u64;
        }
        let queued = if to_pending { self.pending.len() } else { self.relayed.len() };
        if queued + entries.len() > cap {
            return false; // the origin hedges
        }
        for i in &entries {
            if self.mine(i)
                && let Some(o) = self.own.get_mut(&i.entry.id)
            {
                o.copied = true;
            }
        }
        self.changes += entries.iter().filter(|i| is_change(i)).count();
        if to_pending {
            self.pending.extend(entries);
        } else {
            self.foreign += entries.iter().filter(|i| i.entry.origin != self.me).count();
            self.relayed.extend(entries);
        }
        true
    }

    /// Hands everything pending to the target unless this replica proposes it `direct`ly:
    /// own entries are tracked from `now`. Returns the entries to send. Afterwards nothing is
    /// pending unless `direct` (E5).
    pub(super) fn route(&mut self, direct: bool, now: Timestamp) -> Vec<Item> {
        if direct || self.pending.is_empty() {
            return Vec::new();
        }
        let entries: Vec<Item> = self.pending.drain(..).collect();
        self.changes -= entries.iter().filter(|i| is_change(i)).count();
        let (me, inc) = (self.me, self.incarnation);
        for i in entries.iter().filter(|i| i.entry.origin == me && i.entry.incarnation == inc) {
            let id = i.entry.id;
            let _earlier = self.forwarded.insert(id, (i.clone(), now));
            #[cfg(feature = "model")]
            if _earlier.is_some_and(|(_, at)| at < now) {
                self.probes.d2_restarts += 1;
            }
            if let Some(o) = self.own.get_mut(&id) {
                // A copy forwarded over the entry restarts its clock (D2, D3).
                match o.at {
                    At::Pending => o.at = At::Forwarded { since: now },
                    At::Forwarded { .. } => (o.at, o.copied) = (At::Forwarded { since: now }, true),
                    _ => o.copied = true,
                }
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
        let seq = self.next_batch;
        let mut entries = batch::Builder::new(max_bytes);
        // `relayed` drains before `pending`: the first `from_relayed` entries taken came from it.
        let mut from_relayed = 0;
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
            from_relayed += usize::from(relayed);
            entries.push(e);
        }
        if entries.is_empty() {
            return None;
        }
        let items = entries.take();
        self.batched(&items, from_relayed, seq);
        Some(self.new_batch(items))
    }

    /// The own entries among `items` (the first `from_relayed` taken from `relayed`, the rest
    /// from `pending`) moved to batch `seq`. Nothing reads `own` while `cut` takes them, so the
    /// moves wait until it is done: own ids ascend in a cut but for copies, and one ordered
    /// walk replaces a lookup per entry on the leader's commit path.
    fn batched(&mut self, items: &[Item], from_relayed: usize, seq: u64) {
        let (me, inc) = (self.me, self.incarnation);
        let mut mine = items
            .iter()
            .enumerate()
            .filter(|(_, i)| i.entry.origin == me && i.entry.incarnation == inc)
            .map(|(n, i)| (i.entry.id, if n < from_relayed { At::Relayed } else { At::Pending }));
        let Some(first) = mine.clone().next() else { return };
        let to = At::Batched { seq };
        let ascending = mine.clone().zip(mine.clone().skip(1)).all(|((a, _), (b, _))| a < b);
        if !ascending {
            for (id, from) in mine {
                self.moved(id, from, to);
            }
            return;
        }
        let mut next = mine.next();
        for (id, o) in self.own.range_mut(first.0..) {
            while let Some((want, _)) = next
                && want < *id
            {
                next = mine.next();
            }
            let Some((want, from)) = next else { break };
            if want == *id {
                if o.at.same(from) {
                    o.at = to
                } else {
                    o.copied = true
                }
                next = mine.next();
            }
        }
    }

    /// A new local batch of `entries`, not yet in a slot.
    pub(super) fn new_batch(&mut self, entries: Vec<Item>) -> u64 {
        let seq = self.next_batch;
        self.next_batch += 1;
        let key = ValueKey { origin: self.me, incarnation: self.incarnation, seq };
        let ids = entries.iter().map(Item::key).collect();
        let changes = entries.iter().filter(|i| is_change(i)).count();
        let value = batch::encode(&entries);
        let b = LocalBatch { key, value, entries: ids, slot: None, changes, handed: false };
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

    /// An own entry is resolved (committed, superseded, or internal): no longer handed to
    /// anyone nor awaited.
    pub(super) fn resolved(&mut self, id: ProposalId) -> Option<Left> {
        self.forwarded.remove(&id);
        self.leave(id)
    }

    /// Own batch `seq` was decided and applied.
    pub(super) fn applied(&mut self, seq: u64) {
        self.remove_batch(seq);
    }

    /// Entries committed: a batch waiting for a slot may hold nothing else.
    pub(super) fn dedup_grew(&mut self) {
        self.prune_due |= !self.unslotted.is_empty();
    }
}

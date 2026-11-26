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

use crate::batch::{self, DataId, Item, Kind, Piece, Ref};
use crate::dedup::Dedup;
use crate::proposal::ValueKey;
use crate::types::{NodeId, ProposalId, Slot, Timestamp};

/// A batch this replica proposes, in at most one slot at a time until it is decided.
#[cfg_attr(feature = "model", derive(Clone, Hash))]
pub(super) struct LocalBatch {
    pub(super) key: ValueKey,
    pub(super) value: Bytes,
    pub(super) entries: Vec<(NodeId, u64, ProposalId)>,
    pub(super) refs: Vec<Ref>,
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
    pending: VecDeque<Piece>,
    relayed: VecDeque<Piece>,
    forwarded: BTreeMap<ProposalId, (Item, Timestamp)>,
    batches: BTreeMap<u64, LocalBatch>,
    own: BTreeMap<ProposalId, Own>,
    next_batch: u64,
    /// Batches with entries and no slot, which `drive` proposes before cutting a new one.
    unslotted: BTreeSet<u64>,
    /// Entries and references of other members in `relayed` and in batches.
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

fn piece_is_change(p: &Piece) -> bool {
    matches!(p, Piece::Entry(i) if is_change(i))
}

fn piece_origin(p: &Piece) -> NodeId {
    match p {
        Piece::Entry(i) => i.entry.origin,
        Piece::Ref(r) => r.id.origin,
    }
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

    fn foreign_in(&self, b: &LocalBatch) -> usize {
        b.entries.iter().filter(|&&(o, _, _)| o != self.me).count()
            + b.refs.iter().filter(|r| r.id.origin != self.me).count()
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

    fn push_pending(&mut self, p: Piece) {
        self.changes += usize::from(piece_is_change(&p));
        self.pending.push_back(p);
    }

    fn push_relayed(&mut self, p: Piece) {
        self.changes += usize::from(piece_is_change(&p));
        self.foreign += usize::from(piece_origin(&p) != self.me);
        self.relayed.push_back(p);
    }

    fn pop(&mut self, relayed: bool) -> Option<Piece> {
        let p = if relayed { self.relayed.pop_front() } else { self.pending.pop_front() }?;
        self.changes -= usize::from(piece_is_change(&p));
        self.foreign -= usize::from(relayed && piece_origin(&p) != self.me);
        Some(p)
    }

    fn insert_batch(&mut self, seq: u64, b: LocalBatch) {
        self.foreign += self.foreign_in(&b);
        self.changes += b.changes;
        if b.slot.is_none() && (!b.entries.is_empty() || !b.refs.is_empty()) {
            self.unslotted.insert(seq);
        }
        self.batches.insert(seq, b);
    }

    fn remove_batch(&mut self, seq: u64) -> Option<LocalBatch> {
        let b = self.batches.remove(&seq)?;
        self.foreign -= self.foreign_in(&b);
        self.changes -= b.changes;
        self.unslotted.remove(&seq);
        Some(b)
    }

    /// Whether any queued item or local batch still references data batch `id`.
    pub(super) fn contains_ref(&self, id: DataId) -> bool {
        let has = |p: &Piece| matches!(p, Piece::Ref(r) if r.id == id);
        self.pending.iter().any(has)
            || self.relayed.iter().any(has)
            || self.batches.values().any(|b| b.refs.iter().any(|r| r.id == id))
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

    /// Whether any entry waits in `pending`.
    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Whether `pending` holds a control entry or already fills a batch of `limit` bytes.
    pub(super) fn pending_urgent(&self, limit: usize) -> bool {
        let mut bytes = 0usize;
        for p in &self.pending {
            if matches!(p, Piece::Entry(i) if i.kind != Kind::App) {
                return true;
            }
            bytes = bytes.saturating_add(batch::piece_weight(p));
            if bytes >= limit {
                return true;
            }
        }
        false
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

    /// Own entry `id`, if it waits on the target.
    pub(super) fn forwarded_item(&self, id: ProposalId) -> Option<&Item> {
        self.forwarded.get(&id).map(|(i, _)| i)
    }

    /// The key of the value batch `seq` proposes.
    pub(super) fn batch_key(&self, seq: u64) -> Option<ValueKey> {
        self.batches.get(&seq).map(|b| b.key)
    }

    /// The value of the local batch proposed under `key`.
    pub(super) fn batch_value(&self, key: ValueKey) -> Option<&Bytes> {
        self.batches.get(&key.seq).filter(|b| b.key == key).map(|b| &b.value)
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
        self.push_pending(Piece::Entry(item));
    }

    /// A follower handed pieces (inline entries and references) to this replica: into
    /// `pending` (to hand on, or as the leader) or `relayed` (as a deputy), unless that
    /// queue would exceed `cap`. An own entry among them is a copy.
    pub(super) fn received(&mut self, pieces: Vec<Piece>, to_pending: bool, cap: usize) -> bool {
        #[cfg(feature = "model")]
        {
            let own = pieces.iter().filter(|p| matches!(p, Piece::Entry(i) if self.mine(i)));
            self.probes.d3 += own.count() as u64;
        }
        let queued = if to_pending { self.pending.len() } else { self.relayed.len() };
        if queued + pieces.len() > cap {
            return false; // the origin hedges
        }
        for p in &pieces {
            if let Piece::Entry(i) = p
                && self.mine(i)
                && let Some(o) = self.own.get_mut(&i.entry.id)
            {
                o.copied = true;
            }
        }
        self.changes += pieces.iter().filter(|p| piece_is_change(p)).count();
        if to_pending {
            self.pending.extend(pieces);
        } else {
            self.foreign += pieces.iter().filter(|p| piece_origin(p) != self.me).count();
            self.relayed.extend(pieces);
        }
        true
    }

    /// Hands everything pending to the target unless this replica proposes it `direct`ly:
    /// own entries are tracked from `now`. Returns the pieces to send. Afterwards nothing is
    /// pending unless `direct` (or the caller deferred routing under forward coalescing; E5).
    pub(super) fn route(&mut self, direct: bool, now: Timestamp) -> Vec<Piece> {
        if direct || self.pending.is_empty() {
            return Vec::new();
        }
        let pieces: Vec<Piece> = self.pending.drain(..).collect();
        self.changes -= pieces.iter().filter(|p| piece_is_change(p)).count();
        let (me, inc) = (self.me, self.incarnation);
        for p in &pieces {
            let Piece::Entry(i) = p else { continue };
            if i.entry.origin != me || i.entry.incarnation != inc {
                continue;
            }
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
        pieces
    }

    /// Batches whose entries all committed elsewhere are dead weight. Returns any data batch
    /// ids that the pruned batches referenced so the caller can release them.
    pub(super) fn prune(&mut self, dedup: &Dedup, ref_committed: impl Fn(Ref) -> bool) -> Vec<DataId> {
        if !self.prune_due {
            return Vec::new();
        }
        self.prune_due = false;
        let dead: Vec<u64> = self
            .batches
            .iter()
            .filter(|(_, b)| {
                b.slot.is_none()
                    && !b.entries.iter().any(|&(o, i, id)| !dedup.contains(o, i, id))
                    && b.refs.iter().all(|&r| ref_committed(r))
            })
            .map(|(seq, _)| *seq)
            .collect();
        let mut released = Vec::new();
        for seq in dead {
            if let Some(b) = self.remove_batch(seq) {
                released.extend(b.refs.into_iter().map(|r| r.id));
            }
        }
        released
    }

    /// Cuts the next batch: relayed pieces always, pending ones only when proposing them
    /// directly (`lead`). Returns the new batch sequence (if any) and the data batch ids of
    /// any already-committed references skipped from the queues.
    pub(super) fn cut(
        &mut self,
        lead: bool,
        max_bytes: usize,
        dedup: &Dedup,
        ref_committed: impl Fn(Ref) -> bool,
    ) -> (Option<u64>, Vec<DataId>) {
        let seq = self.next_batch;
        let mut pieces = batch::PieceBuilder::new(max_bytes);
        let mut skipped_refs = Vec::new();
        // `relayed` drains before `pending`: the first `from_relayed` pieces taken came from it.
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
            let Some(p) = queue.front() else { break };
            if !pieces.fits(p) {
                break;
            }
            let Some(p) = self.pop(relayed) else { break };
            match &p {
                Piece::Entry(e) => {
                    let (o, inc, id) = e.key();
                    if dedup.contains(o, inc, id) {
                        continue;
                    }
                }
                Piece::Ref(r) => {
                    if ref_committed(*r) {
                        skipped_refs.push(r.id);
                        continue;
                    }
                }
            }
            from_relayed += usize::from(relayed);
            pieces.push(p);
        }
        if pieces.is_empty() {
            return (None, skipped_refs);
        }
        let items = pieces.take();
        self.batched(&items, from_relayed, seq);
        (Some(self.new_batch(items)), skipped_refs)
    }

    /// The own entries among `pieces` (the first `from_relayed` taken from `relayed`, the rest
    /// from `pending`) moved to batch `seq`. Nothing reads `own` while `cut` takes them, so the
    /// moves wait until it is done: own ids ascend in a cut but for copies, and one ordered
    /// walk replaces a lookup per entry on the leader's commit path.
    fn batched(&mut self, pieces: &[Piece], from_relayed: usize, seq: u64) {
        let (me, inc) = (self.me, self.incarnation);
        let mut mine = pieces.iter().enumerate().filter_map(|(n, p)| match p {
            Piece::Entry(i) if i.entry.origin == me && i.entry.incarnation == inc => {
                Some((i.entry.id, if n < from_relayed { At::Relayed } else { At::Pending }))
            }
            _ => None,
        });
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

    /// A new local batch of `pieces`, not yet in a slot.
    pub(super) fn new_batch(&mut self, pieces: Vec<Piece>) -> u64 {
        let seq = self.next_batch;
        self.next_batch += 1;
        let key = ValueKey { origin: self.me, incarnation: self.incarnation, seq };
        let mut ids = Vec::new();
        let mut refs = Vec::new();
        let mut changes = 0;
        for p in &pieces {
            match p {
                Piece::Entry(i) => {
                    ids.push(i.key());
                    changes += usize::from(is_change(i));
                }
                Piece::Ref(r) => refs.push(*r),
            }
        }
        let value = batch::encode_pieces(&pieces);
        let b = LocalBatch { key, value, entries: ids, refs, slot: None, changes, handed: false };
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

    /// Own batch `seq` was decided and applied; returns the references it held.
    pub(super) fn applied(&mut self, seq: u64) -> Vec<Ref> {
        self.remove_batch(seq).map_or_else(Vec::new, |b| b.refs)
    }

    /// Entries committed: a batch waiting for a slot may hold nothing else.
    pub(super) fn dedup_grew(&mut self) {
        self.prune_due |= !self.unslotted.is_empty();
    }
}

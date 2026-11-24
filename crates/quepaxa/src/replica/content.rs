//! Batch content custody, rules R and P, and fetch (#172, `docs/design/batch-ids.md`).
//!
//! Large chunks of forwarded application entries travel directly from their origin to every
//! replica as `Body::Data`, while `Forward` and slot values carry compact `Ref` items.
//! Rule R holds back record requests that would change a register until their content is
//! present and writes `Record::Data` ahead of `Record::Recorder`. Rule P holds back a
//! proposer's record requests until it holds every reference in its working value. Missing
//! content is fetched from the requester/sender first, then rotated across the origin and
//! the slot's members once per `retransmit_interval`.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use bytes::Bytes;

use super::Replica;
use crate::batch::{self, DataId, Item, Piece, Ref};
use crate::codec::Body;
use crate::content::PinError;
use crate::crc::crc32c;
use crate::durable::Record;
use crate::proposal::Proposal;
use crate::types::{MAX_SLOT, NodeId, ProposalId, Slot, Timestamp};

/// A record request held back by rule R until its referenced data batches arrive.
#[cfg_attr(feature = "model", derive(Clone, Hash))]
struct Parked {
    step: u32,
    proposal: Proposal,
}

/// A `Forward` carrying references whose content has not arrived yet: staged here briefly
/// instead of entering `entries` so a crashed origin's lost `Data` can never trap a local
/// batch or wedge a slot.
#[cfg_attr(feature = "model", derive(Clone, Hash))]
struct PendingForward {
    from: NodeId,
    seq: u64,
    to_pending: bool,
    pieces: Vec<Piece>,
    expires_at: Timestamp,
}

/// An outstanding `Fetch` for one data batch: at most one per `DataId`.
#[cfg_attr(feature = "model", derive(Clone, Hash))]
struct InFlightFetch {
    /// Lowest slot currently needing this batch (for `Compacted` detection).
    slot: Slot,
    len: u32,
    crc: u32,
    /// First peer to ask (requester for rule R, sender for `Decided` or adoption).
    first: NodeId,
    /// Rotation index across `[first, origin, ...slot_members]`.
    attempt: u32,
    sent_at: Timestamp,
}

/// Content dissemination, rule-R parking, and fetch tracking for one [`Replica`].
#[derive(Default)]
#[cfg_attr(feature = "model", derive(Clone, Hash))]
pub(super) struct Fetching {
    next_data: u64,
    /// Data batches originated by this replica, kept until every entry in them commits.
    own_data: BTreeMap<DataId, Vec<(NodeId, u64, ProposalId)>>,
    /// Rule-R parked record requests, at most one per `(slot, requester)` in the window.
    parked: BTreeMap<(Slot, NodeId), Parked>,
    /// Received forwards waiting briefly for out-of-order `Data` before entering `entries`.
    pending_forwards: Vec<PendingForward>,
    /// Outstanding fetches, at most one per `DataId`.
    fetches: BTreeMap<DataId, InFlightFetch>,
    /// Decided slots in `log` still missing at least one referenced batch, with who reported them.
    missing_log: BTreeMap<Slot, Option<NodeId>>,
}

impl Replica {
    /// Encodes a chunk of application entries; if larger than `inline_batch_bytes`, sends it
    /// as `Body::Data` to every member in `schedule.everyone()` and returns a single `Ref`.
    pub(super) fn disseminate_chunk(&mut self, items: Vec<Item>) -> Vec<Piece> {
        let bytes = batch::encode(&items);
        if bytes.len() <= self.cfg.inline_batch_bytes as usize {
            return items.into_iter().map(Piece::Entry).collect();
        }
        let seq = self.fetching.next_data;
        self.fetching.next_data += 1;
        let id = DataId { origin: self.me, incarnation: self.incarnation, seq };
        let len = bytes.len() as u32;
        let crc = crc32c(&bytes);
        self.content.insert(id, bytes.clone());
        self.content.keep(id);
        self.fetching.own_data.insert(id, items.iter().map(Item::key).collect());
        let peers: Vec<NodeId> = self.schedule.everyone().into_iter().filter(|&m| m != self.me).collect();
        for peer in peers {
            self.metrics.data_sent += 1;
            self.send(peer, Body::Data { id, bytes: bytes.clone() });
        }
        alloc::vec![Piece::Ref(Ref { id, len, crc })]
    }

    /// Checks all references in `value` against the content store without mutating it.
    /// Returns `Err(())` if any reference mismatches held bytes, else the missing references.
    pub(super) fn check_refs(&self, value: &Bytes) -> Result<Vec<Ref>, ()> {
        let mut missing = Vec::new();
        for r in batch::refs(value) {
            match self.content.check(r) {
                Ok(()) => {}
                Err(PinError::Missing) => missing.push(r),
                Err(PinError::Mismatch) => return Err(()),
            }
        }
        Ok(missing)
    }

    /// Pins every held reference of `value` for `slot` and lazily writes its `Record::Data`
    /// through the gate if not yet written in this incarnation's durable view. Returns
    /// `Err(())` on mismatch, else any references not yet held.
    pub(super) fn pin_and_write_refs(&mut self, value: &Bytes, slot: Slot) -> Result<Vec<Ref>, ()> {
        let missing = self.check_refs(value)?;
        for r in batch::refs(value) {
            if self.content.pin(r, slot).is_ok()
                && let Some(bytes) = self.content.take_unwritten(&r.id)
            {
                self.gate.write(Record::Data { id: r.id, bytes }.encode());
            }
        }
        Ok(missing)
    }

    /// Pins every held reference of `value` for `slot` (proposer rule P: keeps content in
    /// memory without forcing a disk write until a recorder or decision needs it).
    pub(super) fn pin_refs_for_proposer(&mut self, value: &Bytes, slot: Slot) -> Result<Vec<Ref>, ()> {
        let missing = self.check_refs(value)?;
        if missing.is_empty() {
            for r in batch::refs(value) {
                let _ = self.content.pin(r, slot);
            }
        } else {
            for r in batch::refs(value) {
                self.content.keep(r.id);
            }
        }
        Ok(missing)
    }

    /// Parks a record request missing content (rule R) and starts fetching its references.
    pub(super) fn park_request(
        &mut self,
        slot: Slot,
        from: NodeId,
        step: u32,
        proposal: Proposal,
        missing: &[Ref],
    ) {
        if self.fetching.parked.get(&(slot, from)).is_some_and(|old| step < old.step) {
            return;
        }
        for r in batch::refs(&proposal.value) {
            self.content.keep(r.id);
        }
        if let Some(old) = self.fetching.parked.insert((slot, from), Parked { step, proposal }) {
            for r in batch::refs(&old.proposal.value) {
                self.maybe_release_content(r.id);
            }
        }
        self.metrics.parked_requests += 1;
        self.fetch_refs(slot, from, missing);
    }

    /// Drops any parked requests for `slot` (once decided).
    pub(super) fn clear_parked_slot(&mut self, slot: Slot) {
        let mut released = Vec::new();
        self.fetching.parked.retain(|(s, _), p| {
            if *s == slot {
                released.extend(batch::refs(&p.proposal.value).into_iter().map(|r| r.id));
                false
            } else {
                true
            }
        });
        for id in released {
            self.maybe_release_content(id);
        }
    }

    /// The value of a parked request for `slot` under `key`, if any, so `on_decided` can
    /// resolve a decision immediately without a `CatchUp` round trip.
    pub(super) fn parked_value(&self, slot: Slot, key: crate::proposal::ValueKey) -> Option<&Bytes> {
        self.fetching
            .parked
            .range((slot, NodeId(0))..=(slot, NodeId(u32::MAX)))
            .find(|(_, p)| p.proposal.key == key)
            .map(|(_, p)| &p.proposal.value)
    }

    /// Notes that decided `slot` still lacks some referenced content.
    pub(super) fn note_missing_slot(&mut self, slot: Slot, hint: Option<NodeId>) {
        self.fetching.missing_log.entry(slot).or_insert(hint);
    }

    /// Returns the peer that reported decided `slot`, if recorded.
    pub(super) fn decided_hint(&self, slot: Slot) -> Option<NodeId> {
        self.fetching.missing_log.get(&slot).copied().flatten()
    }

    /// Candidate holders for a fetch: `first` (requester/sender), then `origin`, then the
    /// slot's members in order, deduplicated and excluding `self.me`.
    fn fetch_ring(&self, slot: Slot, origin: NodeId, first: NodeId) -> Vec<NodeId> {
        let mut ring = Vec::new();
        let mut push = |n: NodeId| {
            if n != self.me && !ring.contains(&n) {
                ring.push(n);
            }
        };
        push(first);
        push(origin);
        for &m in self.schedule.members(slot) {
            push(m);
        }
        ring
    }

    /// Starts a `Fetch` for each reference in `missing` that does not already have one in
    /// flight, asking `first` first.
    pub(super) fn fetch_refs(&mut self, slot: Slot, first: NodeId, missing: &[Ref]) {
        let mut by_target: BTreeMap<NodeId, Vec<DataId>> = BTreeMap::new();
        for &r in missing {
            if let Some(in_flight) = self.fetching.fetches.get_mut(&r.id) {
                in_flight.slot = in_flight.slot.min(slot);
                continue;
            }
            let ring = self.fetch_ring(slot, r.id.origin, first);
            let Some(&target) = ring.first() else { continue };
            self.fetching.fetches.insert(
                r.id,
                InFlightFetch { slot, len: r.len, crc: r.crc, first, attempt: 0, sent_at: self.now },
            );
            by_target.entry(target).or_default().push(r.id);
        }
        for (target, ids) in by_target {
            self.metrics.fetches += ids.len() as u64;
            self.send(target, Body::Fetch { slot, ids });
        }
    }

    /// Stages a forward carrying references whose content has not yet arrived, and fetches
    /// those missing references from `from`. If the content does not arrive within two
    /// retransmit intervals (e.g. the origin crashed before anyone received `Data`), the
    /// staged forward expires without ever entering `entries` or wedging a slot.
    pub(super) fn stage_forward(
        &mut self,
        from: NodeId,
        seq: u64,
        to_pending: bool,
        pieces: Vec<Piece>,
        missing: &[Ref],
    ) {
        let expires_at = self.now.after(self.cfg.retransmit_interval.saturating_mul(2));
        if let Some(existing) =
            self.fetching.pending_forwards.iter_mut().find(|pf| pf.from == from && pf.seq == seq)
        {
            existing.expires_at = expires_at;
            self.fetch_refs(Slot(MAX_SLOT), from, missing);
            return;
        }
        let cap = (self.cfg.max_in_flight_slots as usize)
            .saturating_mul(self.schedule.members(Slot(self.committed)).len().max(1))
            .max(16);
        if self.fetching.pending_forwards.len() >= cap {
            return;
        }
        for p in &pieces {
            if let Piece::Ref(r) = *p {
                self.content.keep(r.id);
            }
        }
        self.fetching.pending_forwards.push(PendingForward { from, seq, to_pending, pieces, expires_at });
        // Use MAX_SLOT so a holder never mistakes a fresh forward's fetch for a compacted slot.
        self.fetch_refs(Slot(MAX_SLOT), from, missing);
    }

    /// Handles an inbound `Body::Data { id, bytes }`.
    pub(super) fn on_data(&mut self, _from: NodeId, id: DataId, bytes: Bytes) {
        if id.seq == u64::MAX {
            self.metrics.messages_rejected += 1;
            return;
        }
        if let Some(f) = self.fetching.fetches.get(&id)
            && (bytes.len() != f.len as usize || crc32c(&bytes) != f.crc)
        {
            self.metrics.messages_rejected += 1;
            return;
        }
        self.content.insert_untrimmed(id, bytes);
        self.fetching.fetches.remove(&id);
        self.wake_pending_forwards();
        self.wake_missing_log(id);
        self.wake_parked(id);
        self.wake_proposers(id);
        if self.still_kept(id) {
            self.content.keep(id);
        } else {
            self.content.release(id);
        }
        self.content.trim_origin(id.origin);
    }

    fn wake_pending_forwards(&mut self) {
        if self.fetching.pending_forwards.is_empty() {
            return;
        }
        let staged = core::mem::take(&mut self.fetching.pending_forwards);
        let mut keep_staged = Vec::new();
        let mut released = Vec::new();
        let cap =
            self.cfg.max_outstanding_proposals as usize * self.schedule.members(Slot(self.committed)).len();
        for pf in staged {
            let mut missing = false;
            let mut mismatch = false;
            for p in &pf.pieces {
                if let Piece::Ref(r) = *p {
                    match self.content.check(r) {
                        Ok(()) => {}
                        Err(PinError::Missing) => missing = true,
                        Err(PinError::Mismatch) => mismatch = true,
                    }
                }
            }
            let refs: Vec<DataId> = pf
                .pieces
                .iter()
                .filter_map(|p| match *p {
                    Piece::Ref(r) => Some(r.id),
                    Piece::Entry(_) => None,
                })
                .collect();
            if mismatch {
                self.metrics.messages_rejected += 1;
                released.extend(refs);
                continue;
            }
            if missing {
                keep_staged.push(pf);
                continue;
            }
            if self.entries.received(pf.pieces, pf.to_pending, cap) {
                for id in refs {
                    self.content.keep(id);
                }
                self.watch.fwd.took(pf.from, pf.seq);
                self.ack_forward(pf.from);
            } else {
                released.extend(refs);
            }
        }
        self.fetching.pending_forwards = keep_staged;
        for id in released {
            self.maybe_release_content(id);
        }
    }

    fn wake_missing_log(&mut self, id: DataId) {
        if self.fetching.missing_log.is_empty() {
            return;
        }
        let slots: Vec<Slot> = self
            .fetching
            .missing_log
            .keys()
            .copied()
            .filter(|s| {
                self.log.get(s).is_some_and(|d| batch::refs(&d.proposal.value).iter().any(|r| r.id == id))
            })
            .collect();
        if slots.is_empty() {
            return;
        }
        for slot in slots {
            let Some(value) = self.log.get(&slot).map(|d| d.proposal.value.clone()) else {
                self.fetching.missing_log.remove(&slot);
                continue;
            };
            match self.pin_and_write_refs(&value, slot) {
                Ok(missing) if !missing.is_empty() => {}
                _ => {
                    self.fetching.missing_log.remove(&slot);
                }
            }
        }
        self.advance_held();
        self.emit_commits();
    }

    fn wake_parked(&mut self, id: DataId) {
        if self.fetching.parked.is_empty() {
            return;
        }
        let ready: Vec<(Slot, NodeId)> = self
            .fetching
            .parked
            .iter()
            .filter(|(_, p)| {
                let refs = batch::refs(&p.proposal.value);
                refs.iter().any(|r| r.id == id)
                    && self.check_refs(&p.proposal.value).map_or(true, |m| m.is_empty())
            })
            .map(|(k, _)| *k)
            .collect();
        for key in ready {
            if let Some(p) = self.fetching.parked.remove(&key) {
                let (slot, from) = key;
                let refs = batch::refs(&p.proposal.value);
                self.on_request(from, slot, p.step, p.proposal);
                for r in refs {
                    self.maybe_release_content(r.id);
                }
            }
        }
    }

    fn wake_proposers(&mut self, id: DataId) {
        let ready: Vec<Slot> = self
            .proposers
            .iter()
            .filter(|(_, a)| {
                a.waiting_content && {
                    let val = &a.prop.working().value;
                    batch::refs(val).iter().any(|r| r.id == id)
                        && self.check_refs(val).map_or(true, |m| m.is_empty())
                }
            })
            .map(|(s, _)| *s)
            .collect();
        for slot in ready {
            let refs =
                self.proposers.get(&slot).map(|a| batch::refs(&a.prop.working().value)).unwrap_or_default();
            self.send_requests(slot, false);
            for r in refs {
                self.maybe_release_content(r.id);
            }
        }
    }

    /// Handles an inbound `Body::Fetch { slot, ids }`.
    pub(super) fn on_fetch(&mut self, from: NodeId, slot: Slot, ids: Vec<DataId>) {
        if slot.0 > MAX_SLOT || ids.is_empty() {
            self.metrics.messages_rejected += 1;
            return;
        }
        let mut missing = false;
        for id in ids {
            if let Some(bytes) = self.content.get(&id).cloned() {
                self.metrics.fetches_served += 1;
                self.send(from, Body::Data { id, bytes });
            } else {
                missing = true;
            }
        }
        if missing && slot.0 < self.floor {
            self.send(from, Body::Compacted { through: Slot(self.floor - 1) });
        }
    }

    /// Handles an inbound `Body::Entries { first, proposals, content }`: stages content per
    /// slot so multi-slot pages never evict earlier batches before `learn` pins them.
    pub(super) fn on_entries(
        &mut self,
        from: NodeId,
        first: Slot,
        proposals: Vec<Proposal>,
        content: Vec<(DataId, Bytes)>,
    ) {
        let empty = proposals.is_empty();
        let map: BTreeMap<DataId, Bytes> = content.into_iter().collect();
        for (i, p) in proposals.into_iter().enumerate() {
            let Some(slot) = first.0.checked_add(i as u64) else { break };
            for r in batch::refs(&p.value) {
                if let Some(bytes) = map.get(&r.id)
                    && bytes.len() == r.len as usize
                    && crc32c(bytes) == r.crc
                {
                    self.content.insert_untrimmed(r.id, bytes.clone());
                }
            }
            self.learn(Slot(slot), p, false, Some(from));
        }
        for (id, bytes) in map {
            self.on_data(from, id, bytes);
        }
        self.check_probe_answers();
        if let Some(peer) = self.catch_up.page(from, empty, self.committed, self.now) {
            self.request_catch_up(peer);
        }
    }

    /// Extends the held prefix: contiguous decided slots from `floor` whose referenced
    /// content is all present and written to the durable log.
    pub(super) fn advance_held(&mut self) {
        while let Some(d) = self.log.get(&Slot(self.held)) {
            let ready = batch::refs(&d.proposal.value)
                .into_iter()
                .all(|r| self.content.check(r).is_ok() && self.content.is_written(&r.id));
            if !ready {
                break;
            }
            self.held += 1;
        }
    }

    /// Retries due fetches, rotating through `[first, origin, ...slot_members]`, and drops
    /// expired staged forwards and fetches nothing still waits for.
    pub(super) fn retry_fetches(&mut self) {
        let now = self.now;
        let mut dropped_refs = Vec::new();
        self.fetching.pending_forwards.retain(|pf| {
            if pf.expires_at > now {
                true
            } else {
                for p in &pf.pieces {
                    if let Piece::Ref(r) = *p {
                        dropped_refs.push(r.id);
                    }
                }
                false
            }
        });
        for id in dropped_refs {
            self.maybe_release_content(id);
        }
        if self.fetching.fetches.is_empty() {
            return;
        }
        let needed = self.needed_data_ids();
        let floor = self.floor;
        self.fetching
            .fetches
            .retain(|id, f| f.slot.0 >= floor && self.content.get(id).is_none() && needed.contains(id));
        let interval = self.cfg.retransmit_interval;
        let due: Vec<(DataId, Slot, NodeId, u32)> = self
            .fetching
            .fetches
            .iter()
            .filter(|(_, f)| f.sent_at.after(interval) <= now)
            .map(|(id, f)| (*id, f.slot, f.first, f.attempt.wrapping_add(1)))
            .collect();
        let mut by_target: BTreeMap<(Slot, NodeId), Vec<DataId>> = BTreeMap::new();
        for (id, slot, first, attempt) in due {
            let ring = self.fetch_ring(slot, id.origin, first);
            if ring.is_empty() {
                self.fetching.fetches.remove(&id);
                continue;
            }
            let target = ring[attempt as usize % ring.len()];
            if let Some(f) = self.fetching.fetches.get_mut(&id) {
                f.attempt = attempt;
                f.sent_at = now;
            }
            by_target.entry((slot, target)).or_default().push(id);
        }
        for ((slot, target), ids) in by_target {
            self.metrics.fetch_retries += ids.len() as u64;
            self.send(target, Body::Fetch { slot, ids });
        }
    }

    fn needed_data_ids(&self) -> BTreeSet<DataId> {
        let mut needed = BTreeSet::new();
        for slot in self.fetching.missing_log.keys() {
            if let Some(d) = self.log.get(slot) {
                needed.extend(batch::refs(&d.proposal.value).into_iter().map(|r| r.id));
            }
        }
        for p in self.fetching.parked.values() {
            needed.extend(batch::refs(&p.proposal.value).into_iter().map(|r| r.id));
        }
        for pf in &self.fetching.pending_forwards {
            for p in &pf.pieces {
                if let Piece::Ref(r) = *p {
                    needed.insert(r.id);
                }
            }
        }
        for a in self.proposers.values().filter(|a| a.waiting_content) {
            needed.extend(batch::refs(&a.prop.working().value).into_iter().map(|r| r.id));
        }
        needed
    }

    pub(super) fn earliest_fetch(&self) -> Option<Timestamp> {
        let fetches = self.fetching.fetches.values().map(|f| f.sent_at.after(self.cfg.retransmit_interval));
        let staged = self.fetching.pending_forwards.iter().map(|pf| pf.expires_at);
        fetches.chain(staged).min()
    }

    /// Releases any originated data batch whose entries have all committed.
    pub(super) fn release_committed_own_data(&mut self) {
        if self.fetching.own_data.is_empty() {
            return;
        }
        let done: Vec<DataId> = self
            .fetching
            .own_data
            .iter()
            .filter(|(_, keys)| keys.iter().all(|&(o, inc, id)| self.dedup.contains(o, inc, id)))
            .map(|(id, _)| *id)
            .collect();
        for id in done {
            self.fetching.own_data.remove(&id);
            self.maybe_release_content(id);
        }
    }

    /// Whether any originated batch, queued entry, staged forward, parked request or waiting
    /// proposer still needs `id` kept in memory ahead of slot pinning.
    fn still_kept(&self, id: DataId) -> bool {
        self.fetching.own_data.contains_key(&id)
            || self.entries.contains_ref(id)
            || self
                .fetching
                .pending_forwards
                .iter()
                .any(|pf| pf.pieces.iter().any(|p| matches!(p, Piece::Ref(r) if r.id == id)))
            || self
                .fetching
                .parked
                .values()
                .any(|p| batch::refs(&p.proposal.value).iter().any(|r| r.id == id))
            || self
                .proposers
                .values()
                .any(|a| a.waiting_content && batch::refs(&a.prop.working().value).iter().any(|r| r.id == id))
    }

    /// Releases `id` from `Store::keep` once nothing still waits on or queues it.
    pub(super) fn maybe_release_content(&mut self, id: DataId) {
        if !self.still_kept(id) {
            self.content.release(id);
        }
    }

    /// Releases every reference in `value` from `Store::keep` if nothing else still needs it.
    pub(super) fn release_value_refs(&mut self, value: &Bytes) {
        for r in batch::refs(value) {
            self.maybe_release_content(r.id);
        }
    }

    /// Cleans up content tracking when compaction raises the floor.
    pub(super) fn raise_floor_content(&mut self, next: u64) {
        let floor = Slot(next);
        let mut released = Vec::new();
        self.fetching.parked.retain(|(s, _), p| {
            if *s >= floor {
                true
            } else {
                released.extend(batch::refs(&p.proposal.value).into_iter().map(|r| r.id));
                false
            }
        });
        self.fetching.missing_log = self.fetching.missing_log.split_off(&floor);
        self.fetching.fetches.retain(|_, f| f.slot >= floor);
        for id in released {
            self.maybe_release_content(id);
        }
        self.content.raise_floor(next);
        self.release_committed_own_data();
    }

    /// Drops transient fetch and parking state on storage fencing.
    pub(super) fn fence_content(&mut self) {
        self.fetching.parked.clear();
        self.fetching.pending_forwards.clear();
        self.fetching.fetches.clear();
        self.fetching.missing_log.clear();
        self.fetching.own_data.clear();
    }
}

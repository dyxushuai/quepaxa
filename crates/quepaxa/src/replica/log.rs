//! Recorder side and the decided log: answering record requests, learning decisions,
//! serving catch-up, and emitting commits in order.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use bytes::Bytes;

use super::entries::Own;
use super::{Decision, Effect, Replica};
use crate::api::{Action, DropReason};
use crate::batch::{self, Kind};
use crate::codec::{Body, DecodeError, reply_proposal};
use crate::content::PinError;
use crate::durable::Record;
use crate::proposal::{Proposal, ValueKey};
use crate::types::{MAX_SLOT, MAX_STEP, NodeId, ProposalId, Slot, Timestamp};

impl Replica {
    pub(super) fn on_request(&mut self, from: NodeId, slot: Slot, step: u32, proposal: Proposal) {
        if step > MAX_STEP || slot.0 > MAX_SLOT {
            self.metrics.messages_rejected += 1;
            return;
        }
        if slot.0 < self.floor {
            self.send(from, Body::Compacted { through: Slot(self.floor - 1) });
            return;
        }
        if let Some(d) = self.log.get(&slot) {
            let (priority, key) = (d.proposal.priority, d.proposal.key);
            self.send(from, Body::Decided { slot, priority, key });
            return;
        }
        if slot.0 < self.committed || slot.0 >= self.committed + 4 * self.window() {
            return;
        }
        let key = proposal.key;
        let refs = batch::refs(&proposal.value);
        if !refs.is_empty() && self.registers.get(&slot).is_none_or(|reg| reg.would_change(step, &proposal)) {
            // Rule R (#172): apply a changing record only while holding every referenced
            // data batch, writing its `Data` record ahead of `Recorder`.
            let Ok(missing) = self.check_ref_list(&refs) else {
                self.metrics.messages_rejected += 1;
                return;
            };
            if !missing.is_empty() {
                self.park_request(slot, from, step, proposal, &missing);
                return;
            }
            let _ = self.pin_and_write_ref_list(&refs, slot);
        }
        let reg = self.registers.entry(slot).or_default();
        let before = reg.step;
        if reg.record(step, proposal) {
            let rec = Record::encode_recorder(slot, reg);
            self.gate.write(rec);
        }
        let s = reg.summary();
        // Progress on this slot postpones hedging it.
        if s.step != before || !self.seen.contains_key(&slot) {
            self.seen.insert(slot, self.now);
        }
        let body = Body::RecordReply {
            slot,
            req_step: step,
            step: s.step,
            first: s.first.as_ref().map(|p| reply_proposal(p, key)),
            prev_agg: s.prev_agg.as_ref().map(|p| reply_proposal(p, key)),
        };
        if from == self.me {
            self.after_durable(Effect::LocalReply(body));
        } else {
            self.send_gated(from, body);
        }
    }

    /// Serves contiguous decided slots from `start` up to the held prefix, with the content
    /// of their references, within half a message budget. The reply leaves once everything
    /// written so far is durable, and is sent even when empty, so its envelope's
    /// `commit_index` (the held prefix) is durable progress (a removed replica retires on it).
    pub(super) fn on_catch_up(&mut self, to: NodeId, start: Slot) {
        if start.0 < self.floor {
            self.send(to, Body::Compacted { through: Slot(self.floor - 1) });
            return;
        }
        let budget = self.cfg.max_message_bytes as usize / 2;
        let mut size = 0;
        let mut proposals = Vec::new();
        let mut content = Vec::new();
        let mut included = BTreeSet::new();
        for expected in start.0..self.held {
            let Some(d) = self.log.get(&Slot(expected)) else { break };
            let mut slot_size = d.proposal.value.len() + 64;
            let mut extra = Vec::new();
            let mut ok = true;
            for r in batch::refs(&d.proposal.value) {
                if included.contains(&r.id) || extra.iter().any(|(id, _)| *id == r.id) {
                    continue;
                }
                let Some(bytes) = self.content.get_checked(r) else {
                    ok = false;
                    break;
                };
                slot_size += bytes.len() + 25;
                extra.push((r.id, bytes.clone()));
            }
            if !ok {
                break;
            }
            if !proposals.is_empty() && size + slot_size > budget {
                break;
            }
            size += slot_size;
            proposals.push(d.proposal.clone());
            for (id, bytes) in extra {
                included.insert(id);
                content.push((id, bytes));
            }
        }
        self.send_gated(to, Body::Entries { first: start, proposals, content });
    }

    /// A peer announced `slot` decided with the value under `key`.
    pub(super) fn on_decided(&mut self, from: NodeId, slot: Slot, priority: u64, key: ValueKey) {
        if slot.0 < self.emitted || self.log.contains_key(&slot) {
            return;
        }
        // A majority made a value durable without this replica (`aside_at`).
        self.watch.disk.decided_elsewhere(self.now);
        // The target announced a slot it decided itself: it is busy, not stuck (`late_at`).
        // Not its answer to a request of ours (it may only have cached someone else's
        // decision).
        if from == self.target() && !self.proposers.contains_key(&slot) {
            self.watch.progressed = self.now;
        }
        if let Some(value) = self.resolve_decided(slot, key) {
            self.learn(slot, Proposal { priority, key, value }, false, Some(from));
            return;
        }
        if slot.0 > MAX_SLOT || self.unresolved.contains_key(&slot) {
            return;
        }
        // Tracked even far ahead: bulk catch-up stops a slot short of the peers' commit
        // index, and nothing else would ask for it. But only a near slot is asked for at
        // once; far ones usually arrive in the bulk pages first, else on the timer.
        self.unresolved.insert(slot, (from, self.now));
        if slot.0 < self.committed + 4 * self.window() {
            self.send(from, Body::CatchUp { from: slot });
        }
    }

    /// The value a decision names: a key identifies exactly one value, so any copy this
    /// replica holds under it is the decided one.
    fn resolve_decided(&self, slot: Slot, key: ValueKey) -> Option<Bytes> {
        let reg = self.registers.get(&slot).into_iter().flat_map(|r| [&r.first, &r.agg, &r.prev_agg]);
        let held = reg.flatten().find(|p| p.key == key).map(|p| &p.value);
        held.or_else(|| self.proposers.get(&slot).and_then(|a| a.prop.value_of(key)))
            .or_else(|| self.entries.batch_value(key))
            .or_else(|| self.parked_value(slot, key))
            .cloned()
    }

    /// Asks again for decided slots still unresolved after a retransmit interval: at most
    /// one `CatchUp` per peer, from the lowest such slot, each slot's peer rotating through
    /// its members (the decider may be gone).
    pub(super) fn retry_unresolved(&mut self) {
        let (emitted, log) = (self.emitted, &self.log);
        self.unresolved.retain(|s, _| s.0 >= emitted && !log.contains_key(s));
        let now = self.now;
        let due: Vec<(Slot, NodeId)> = self
            .unresolved
            .iter()
            .filter(|(_, (_, t))| t.after(self.cfg.retransmit_interval) <= now)
            .map(|(s, (peer, _))| (*s, *peer))
            .collect();
        let mut asks: BTreeMap<NodeId, Slot> = BTreeMap::new();
        for (slot, peer) in due {
            let members = self.schedule.members(slot);
            let after = members.iter().position(|m| *m == peer).map_or(0, |i| i + 1);
            let next = members.iter().cycle().skip(after).take(members.len()).find(|m| **m != self.me);
            let next = next.copied().unwrap_or(peer);
            self.unresolved.insert(slot, (next, now));
            asks.entry(next).or_insert(slot);
        }
        for (peer, slot) in asks {
            self.metrics.catch_up_retries += 1;
            self.send(peer, Body::CatchUp { from: slot });
        }
    }

    pub(super) fn earliest_unresolved(&self) -> Option<Timestamp> {
        self.unresolved.values().map(|(_, t)| t.after(self.cfg.retransmit_interval)).min()
    }

    pub(super) fn learn(&mut self, slot: Slot, proposal: Proposal, broadcast: bool, hint: Option<NodeId>) {
        self.unresolved.remove(&slot);
        self.clear_parked_slot(slot);
        if slot.0 < self.emitted || slot.0 > MAX_SLOT || self.log.contains_key(&slot) {
            return;
        }
        self.metrics.slots_decided += 1;
        let refs = batch::refs(&proposal.value);
        let missing = match self.pin_and_write_ref_list(&refs, slot) {
            Ok(missing) => missing,
            Err(()) => {
                self.note_mismatched_slot(slot);
                Vec::new()
            }
        };
        self.gate.write(Record::encode_decided(slot, &proposal));
        if let Some(a) = self.proposers.remove(&slot) {
            if a.waiting_content {
                self.release_value_refs(&a.prop.working().value);
            }
            if self.entries.batch_key(a.batch).is_some_and(|k| k != proposal.key) {
                self.lost_slot(a.batch, proposal.key.origin, a.started);
            }
        }
        let origin = proposal.key.origin;
        let (priority, key) = (proposal.priority, proposal.key);
        self.log.insert(slot, Decision { proposal });
        if !missing.is_empty() {
            self.note_missing_slot(slot, hint);
            self.fetch_refs(slot, hint.unwrap_or(origin), &missing);
        }
        self.advance_committed();
        self.advance_held();
        if broadcast {
            self.broadcast_decided(slot, priority, key);
        }
        // A decision is already durable at a majority of recorders, so it is applied before
        // its local record is: a replica that crashes first learns it again from peers.
        self.emit_commits();
        self.after_durable(Effect::DecidedDurable(slot));
    }

    /// Announces `slot` decided to every known replica in the current or pending epoch.
    pub(super) fn broadcast_decided(&mut self, slot: Slot, priority: u64, key: ValueKey) {
        for peer in self.schedule.everyone() {
            if peer != self.me {
                self.send(peer, Body::Decided { slot, priority, key });
            }
        }
    }

    /// Extends the contiguous decided prefix, feeding each newly decided slot to the leader
    /// schedule in order.
    pub(super) fn advance_committed(&mut self) {
        let direct = self.proposes_directly();
        while let Some(d) = self.log.get(&Slot(self.committed)) {
            let slot = Slot(self.committed);
            if let Some(t) = &mut self.tuner {
                t.note_decided(slot, self.now);
            }
            self.schedule.observe(slot, d.proposal.key.origin, batch::is_hedge(&d.proposal.value));
            for (_, payload) in batch::changes(&d.proposal.value) {
                if let Some((base, order)) = crate::schedule::decode_change(&payload) {
                    self.schedule.reconfigure(slot, base, order);
                }
            }
            self.committed += 1;
        }
        if self.schedule.knows(self.me) {
            self.was_member = true;
            self.follow.stop();
        } else if self.outside() && self.cfg.follow_interval > 0 {
            self.follow.arm_if_idle(self.now, self.cfg.follow_interval);
        }
        if direct && !self.proposes_directly() {
            self.hand_on_unstarted();
        }
        self.seen = self.seen.split_off(&Slot(self.committed));
        if !self.outside() {
            // Something past our prefix is decided (or a peer is ahead): keep the next slot
            // driven so a quiet cluster cannot strand us behind.
            let ahead = self.catch_up.ahead_of(self.committed)
                || self.log.last_key_value().is_some_and(|(&s, _)| s.0 >= self.committed);
            if ahead {
                self.seen.entry(Slot(self.committed)).or_insert(self.now);
            }
            // A decided membership change not yet in force needs the slots before it decided even
            // if nobody writes: they are taken over like stalled slots (the leader first), a
            // window at a time.
            let from = self.schedule.epoch().1;
            for slot in self.committed..from.min(self.committed + self.window()) {
                self.seen.entry(Slot(slot)).or_insert(self.now);
            }
        }
    }

    pub(super) fn emit_commits(&mut self) {
        while let Some(d) = self.log.get(&Slot(self.emitted)) {
            let slot = Slot(self.emitted);
            if self.is_missing_slot(slot) {
                let refs = batch::refs(&d.proposal.value);
                let missing: Vec<_> =
                    refs.into_iter().filter(|r| self.content.check(*r) == Err(PinError::Missing)).collect();
                if !missing.is_empty() {
                    let target = self.decided_hint(slot).unwrap_or(d.proposal.key.origin);
                    self.note_missing_slot(slot, Some(target));
                    self.fetch_refs(slot, target, &missing);
                    break;
                }
            }
            let key = d.proposal.key;
            let expanded = Self::expand_batch(&self.content, &d.proposal.value);
            let changes = expanded.is_err().then(|| batch::changes(&d.proposal.value));
            let mut items = match expanded {
                Ok(items) => items,
                Err(()) => {
                    self.metrics.corrupt_batches += 1;
                    Vec::new()
                }
            };
            self.applied_schedule.observe(slot, key.origin, batch::is_hedge(&d.proposal.value));
            // Local changes every replica ignores, reported as such rather than committed.
            let mut superseded = Vec::new();
            // A batch may carry one change twice (a forward delivered twice); the first copy
            // decides, as dedup keeps only it below.
            let mut seen = Vec::new();
            let mut apply_change = |key: (NodeId, u64, ProposalId), payload: &Bytes| {
                if seen.contains(&key) {
                    return;
                }
                seen.push(key);
                let (origin, incarnation, id) = key;
                let Some((base, order)) = crate::schedule::decode_change(payload) else { return };
                match self.applied_schedule.reconfigure(slot, base, order.clone()) {
                    Some((epoch, from)) => {
                        self.actions.push(Action::Reconfigured { epoch, from: Slot(from), members: order });
                    }
                    None if origin == self.me && incarnation == self.incarnation => superseded.push(id),
                    None => {}
                }
            };
            if let Some(changes) = changes {
                for (k, payload) in changes {
                    apply_change(k, &payload);
                }
            } else {
                for i in &items {
                    if i.kind == Kind::Reconfigure {
                        apply_change(i.key(), &i.entry.payload);
                    }
                }
            }
            items.retain(|i| self.dedup.first_time(i.entry.origin, i.entry.incarnation, i.entry.id));
            self.entries.dedup_grew();
            self.release_committed_own_data();
            self.count_writes(&items);
            let mine: Vec<ProposalId> = items
                .iter()
                .filter(|i| i.entry.origin == self.me && i.entry.incarnation == self.incarnation)
                .map(|i| i.entry.id)
                .collect();
            let entries = items.into_iter().filter(|i| i.kind == Kind::App).map(|i| i.entry).collect();
            self.actions.push(Action::Commit { slot, entries });
            for id in mine {
                let own = self.entries.resolved(id).map(|l| l.own);
                self.watch.fwd.forget(id);
                if self.retirement.committed(id) {
                    continue;
                }
                // Placement's own move: applied or ignored, the application never sees it.
                if self.placement.policy.on_resolved(id) {
                    continue;
                }
                if superseded.contains(&id) {
                    if own.is_some() {
                        self.actions.push(Action::Dropped { id, reason: DropReason::Superseded });
                    }
                    continue;
                }
                if let Some(Own { since, kind, hedged, .. }) = own {
                    if kind == Kind::App {
                        self.note_commit_latency(self.now.0.saturating_sub(since.0));
                    }
                    // A hedged entry waited out the hedge delay itself: learning from it
                    // would stretch the delay further each time the leader is slow.
                    if !hedged {
                        self.observe_latency(self.now.0.saturating_sub(since.0));
                    }
                    self.actions.push(Action::Committed { id, slot });
                }
            }
            if key.origin == self.me && key.incarnation == self.incarnation {
                let (refs, batch_info) = self.entries.applied(key.seq);
                for r in refs {
                    self.maybe_release_content(r.id);
                }
                if let Some((weight, enqueued_at, hedge)) = batch_info
                    && !hedge
                    && weight > 0
                    && self.leads_now()
                    && self.proposes_directly()
                {
                    let residence = self.now.0.saturating_sub(enqueued_at.0);
                    let slot_rtt = self.request_rtt.unwrap_or(0);
                    if let Some(w) = self
                        .tuner
                        .as_mut()
                        .and_then(|t| t.on_slot_committed(weight, residence, slot_rtt, self.now))
                    {
                        self.apply_tuned_w(w);
                    }
                }
            }
            self.emitted += 1;
            if self.cfg.placement {
                self.note_applied(slot);
            }
        }
    }

    fn expand_batch(content: &crate::content::Store, value: &Bytes) -> Result<Vec<batch::Item>, ()> {
        batch::decode_expanded(value, |r| {
            let bytes = content.get_checked(r).ok_or(DecodeError::BadTag)?;
            batch::decode_content(bytes)
        })
        .map_err(|_| ())
    }

    fn observe_latency(&mut self, sample: u64) {
        self.commit_latency = Some(match self.commit_latency {
            None => sample,
            Some(avg) => avg - avg / 8 + sample / 8,
        });
    }
}

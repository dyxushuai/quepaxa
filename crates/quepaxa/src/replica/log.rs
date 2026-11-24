//! Recorder side and the decided log: answering record requests, learning decisions,
//! serving catch-up, and emitting commits in order.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use bytes::Bytes;

use super::entries::Own;
use super::{Decision, Effect, Replica};
use crate::api::{Action, DropReason};
use crate::batch::{self, Kind};
use crate::codec::{Body, reply_proposal};
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
        let reg = self.registers.entry(slot).or_default();
        let before = reg.step;
        if reg.record(step, proposal) {
            let rec = Record::Recorder { slot, reg: reg.clone() }.encode();
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

    /// Serves contiguous decided slots from `start`, within half a message budget. The reply
    /// leaves once everything written so far is durable, and is sent even when empty, so its
    /// envelope's `commit_index` is durable progress (a removed replica retires on it).
    pub(super) fn on_catch_up(&mut self, to: NodeId, start: Slot) {
        if start.0 < self.floor {
            self.send(to, Body::Compacted { through: Slot(self.floor - 1) });
            return;
        }
        let budget = self.cfg.max_message_bytes as usize / 2;
        let mut size = 0;
        let mut proposals = Vec::new();
        // Entries must be contiguous from `start`.
        for (expected, (slot, d)) in (start.0..=u64::MAX).zip(self.log.range(start..)) {
            if slot.0 != expected {
                break;
            }
            size += d.proposal.value.len() + 64;
            if !proposals.is_empty() && size > budget {
                break;
            }
            proposals.push(d.proposal.clone());
        }
        // Values hold no references yet (#172): every page is self-contained without content.
        self.send_gated(to, Body::Entries { first: start, proposals, content: Vec::new() });
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
            self.learn(slot, Proposal { priority, key, value }, false);
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

    pub(super) fn learn(&mut self, slot: Slot, proposal: Proposal, broadcast: bool) {
        self.unresolved.remove(&slot);
        if slot.0 < self.emitted || slot.0 > MAX_SLOT || self.log.contains_key(&slot) {
            return;
        }
        self.metrics.slots_decided += 1;
        self.gate.write(Record::Decided { slot, proposal: proposal.clone() }.encode());
        if let Some(a) = self.proposers.remove(&slot)
            && self.entries.batch_key(a.batch).is_some_and(|k| k != proposal.key)
        {
            self.lost_slot(a.batch, proposal.key.origin, a.started);
        }
        if broadcast {
            // Everyone in the current or a pending epoch: joining replicas catch up early.
            let peers: Vec<NodeId> = self.schedule.everyone().into_iter().filter(|&m| m != self.me).collect();
            for peer in peers {
                self.send(peer, Body::Decided { slot, priority: proposal.priority, key: proposal.key });
            }
        }
        self.log.insert(slot, Decision { proposal });
        self.advance_committed();
        // A decision is already durable at a majority of recorders, so it is applied before
        // its local record is: a replica that crashes first learns it again from peers.
        self.emit_commits();
        self.after_durable(Effect::DecidedDurable(slot));
    }

    /// Extends the contiguous decided prefix, feeding each newly decided slot to the leader
    /// schedule in order.
    pub(super) fn advance_committed(&mut self) {
        let led = self.leads_now();
        while let Some(d) = self.log.get(&Slot(self.committed)) {
            let slot = Slot(self.committed);
            self.schedule.observe(slot, d.proposal.key.origin);
            for (_, payload) in batch::changes(&d.proposal.value) {
                if let Some((base, order)) = crate::schedule::decode_change(&payload) {
                    self.schedule.reconfigure(slot, base, order);
                }
            }
            self.committed += 1;
        }
        if led && !self.leads_now() {
            self.hand_on_unstarted();
        }
        self.seen = self.seen.split_off(&Slot(self.committed));
        // Something past our prefix is decided (or a peer is ahead): keep the next slot
        // driven so a quiet cluster cannot strand us behind.
        let ahead =
            self.catch_up.ahead_of(self.committed) || self.log.range(Slot(self.committed)..).next().is_some();
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

    pub(super) fn emit_commits(&mut self) {
        while let Some(d) = self.log.get(&Slot(self.emitted)) {
            let slot = Slot(self.emitted);
            let key = d.proposal.key;
            self.applied_schedule.observe(slot, key.origin);
            // Local changes every replica ignores, reported as such rather than committed.
            let mut superseded = Vec::new();
            // A batch may carry one change twice (a forward delivered twice); the first copy
            // decides, as dedup keeps only it below.
            let mut seen = Vec::new();
            for (key, payload) in batch::changes(&d.proposal.value) {
                if seen.contains(&key) {
                    continue;
                }
                seen.push(key);
                let (origin, incarnation, id) = key;
                let Some((base, order)) = crate::schedule::decode_change(&payload) else { continue };
                match self.applied_schedule.reconfigure(slot, base, order.clone()) {
                    Some((epoch, from)) => {
                        self.actions.push(Action::Reconfigured { epoch, from: Slot(from), members: order });
                    }
                    None if origin == self.me && incarnation == self.incarnation => superseded.push(id),
                    None => {}
                }
            }
            let mut items = batch::decode(&d.proposal.value).unwrap_or_else(|_| {
                self.metrics.corrupt_batches += 1;
                Vec::new()
            });
            items.retain(|i| self.dedup.first_time(i.entry.origin, i.entry.incarnation, i.entry.id));
            self.entries.dedup_grew();
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
                self.entries.applied(key.seq);
            }
            self.emitted += 1;
            if self.cfg.placement {
                self.note_applied(slot);
            }
        }
    }

    fn observe_latency(&mut self, sample: u64) {
        self.commit_latency = Some(match self.commit_latency {
            None => sample,
            Some(avg) => avg - avg / 8 + sample / 8,
        });
    }
}

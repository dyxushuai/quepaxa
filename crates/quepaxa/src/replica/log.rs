//! Recorder side and the decided log: answering record requests, learning decisions,
//! serving catch-up, and emitting commits in order.

use alloc::vec::Vec;

use super::{Decision, Effect, Replica};
use crate::api::Action;
use crate::batch::{self, Kind};
use crate::codec::{Body, reply_proposal};
use crate::durable::Record;
use crate::proposal::Proposal;
use crate::types::{MAX_SLOT, MAX_STEP, NodeId, ProposalId, Slot};

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
            let proposal = d.proposal.clone();
            self.send(from, Body::Decided { slot, proposal });
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
        self.send_gated(to, Body::Entries { first: start, proposals });
    }

    pub(super) fn learn(&mut self, slot: Slot, proposal: Proposal, broadcast: bool) {
        if slot.0 < self.emitted || slot.0 > MAX_SLOT || self.log.contains_key(&slot) {
            return;
        }
        self.metrics.slots_decided += 1;
        self.gate.write(Record::Decided { slot, proposal: proposal.clone() }.encode());
        if let Some(a) = self.proposers.remove(&slot)
            && self.batches.get(&a.batch).is_some_and(|b| b.key != proposal.key)
        {
            self.lost_slot(a.batch);
        }
        if broadcast {
            // Everyone in the current or a pending epoch: joining replicas catch up early.
            let peers: Vec<NodeId> = self.schedule.everyone().into_iter().filter(|&m| m != self.me).collect();
            for peer in peers {
                self.send(peer, Body::Decided { slot, proposal: proposal.clone() });
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
        while let Some(d) = self.log.get(&Slot(self.committed)) {
            let slot = Slot(self.committed);
            self.schedule.observe(slot, d.proposal.key.origin);
            for (key, payload) in batch::changes(&d.proposal.value) {
                if let Some(order) = crate::schedule::decode_order(&payload) {
                    self.schedule.reconfigure(slot, key, order);
                }
            }
            self.committed += 1;
        }
        self.seen = self.seen.split_off(&Slot(self.committed));
        // Something past our prefix is decided (or a peer is ahead): keep the next slot
        // driven so a quiet cluster cannot strand us behind.
        let ahead =
            self.peer_committed > self.committed || self.log.range(Slot(self.committed)..).next().is_some();
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
            for (change, payload) in batch::changes(&d.proposal.value) {
                if let Some(order) = crate::schedule::decode_order(&payload)
                    && let Some((epoch, from)) =
                        self.applied_schedule.reconfigure(slot, change, order.clone())
                {
                    self.actions.push(Action::Reconfigured { epoch, from: Slot(from), members: order });
                }
            }
            let mut items = batch::decode(&d.proposal.value).unwrap_or_else(|_| {
                self.metrics.corrupt_batches += 1;
                Vec::new()
            });
            items.retain(|i| self.dedup.first_time(i.entry.origin, i.entry.incarnation, i.entry.id));
            let mine: Vec<(ProposalId, Kind)> = items
                .iter()
                .filter(|i| i.entry.origin == self.me && i.entry.incarnation == self.incarnation)
                .map(|i| (i.entry.id, i.kind))
                .collect();
            let entries = items.into_iter().filter(|i| i.kind == Kind::App).map(|i| i.entry).collect();
            self.actions.push(Action::Commit { slot, entries });
            for (id, kind) in mine {
                self.forwarded.remove(&id);
                if self.retire_barrier.is_some_and(|(_, b)| b == id) {
                    self.outstanding.remove(&id);
                    self.confirm_retired();
                    continue;
                }
                if let Some((at, _)) = self.outstanding.remove(&id) {
                    self.observe_latency(self.now.0.saturating_sub(at.0));
                    self.actions.push(if kind == Kind::Barrier {
                        Action::ReadReady { id, slot }
                    } else {
                        Action::Committed { id, slot }
                    });
                }
            }
            if key.origin == self.me && key.incarnation == self.incarnation {
                self.batches.remove(&key.seq);
            }
            self.emitted += 1;
        }
    }

    fn observe_latency(&mut self, sample: u64) {
        self.commit_latency = Some(match self.commit_latency {
            None => sample,
            Some(avg) => avg - avg / 8 + sample / 8,
        });
    }
}

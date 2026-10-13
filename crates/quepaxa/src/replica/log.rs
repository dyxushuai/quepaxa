//! Recorder side and the decided log: answering record requests, learning decisions,
//! serving catch-up, and emitting commits in order.

use alloc::vec::Vec;

use super::{Decision, Effect, Replica};
use crate::api::Action;
use crate::batch;
use crate::codec::{Body, reply_proposal};
use crate::durable::Record;
use crate::proposal::Proposal;
use crate::types::{NodeId, ProposalId, Slot};

impl Replica {
    pub(super) fn on_request(&mut self, from: NodeId, slot: Slot, step: u32, proposal: Proposal) {
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
            self.writes.push(rec);
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

    /// Serves contiguous decided slots from `start`, within half a message budget.
    pub(super) fn on_catch_up(&mut self, to: NodeId, start: Slot) {
        let budget = self.cfg.max_message_bytes as usize / 2;
        let mut size = 0;
        let mut proposals = Vec::new();
        for slot in start.0.. {
            let Some(d) = self.log.get(&Slot(slot)) else { break };
            size += d.proposal.value.len() + 64;
            if !proposals.is_empty() && size > budget {
                break;
            }
            proposals.push(d.proposal.clone());
        }
        if !proposals.is_empty() {
            self.send(to, Body::Entries { first: start, proposals });
        }
    }

    pub(super) fn learn(&mut self, slot: Slot, proposal: Proposal, broadcast: bool) {
        if slot.0 < self.emitted || self.log.contains_key(&slot) {
            return;
        }
        self.metrics.slots_decided += 1;
        self.writes.push(Record::Decided { slot, proposal: proposal.clone() }.encode());
        // A local batch that lost its slot goes back to be proposed elsewhere.
        if let Some(a) = self.proposers.remove(&slot)
            && let Some(b) = self.batches.get_mut(&a.batch)
            && b.key != proposal.key
        {
            b.slot = None;
        }
        for gap in self.committed..slot.0 {
            self.seen.entry(Slot(gap)).or_insert(self.now);
        }
        if broadcast {
            let peers: Vec<NodeId> = self.cfg.members.iter().copied().filter(|&m| m != self.me).collect();
            for peer in peers {
                self.send(peer, Body::Decided { slot, proposal: proposal.clone() });
            }
        }
        self.log.insert(slot, Decision { proposal, durable: false });
        self.advance_committed();
        self.after_durable(Effect::DecidedDurable(slot));
    }

    /// Extends the contiguous decided prefix, feeding each newly decided slot to the leader
    /// schedule in order.
    pub(super) fn advance_committed(&mut self) {
        while let Some(d) = self.log.get(&Slot(self.committed)) {
            self.schedule.observe(Slot(self.committed), d.proposal.key.origin);
            self.committed += 1;
        }
        self.seen = self.seen.split_off(&Slot(self.committed));
    }

    pub(super) fn emit_commits(&mut self) {
        while let Some(d) = self.log.get(&Slot(self.emitted)) {
            if !d.durable {
                break;
            }
            let slot = Slot(self.emitted);
            let key = d.proposal.key;
            let mut items = batch::decode(&d.proposal.value).unwrap_or_else(|_| {
                self.metrics.corrupt_batches += 1;
                Vec::new()
            });
            items.retain(|i| self.dedup.first_time(i.entry.origin, i.entry.incarnation, i.entry.id));
            let mine: Vec<(ProposalId, bool)> = items
                .iter()
                .filter(|i| i.entry.origin == self.me && i.entry.incarnation == self.incarnation)
                .map(|i| (i.entry.id, i.barrier))
                .collect();
            let entries = items.into_iter().filter(|i| !i.barrier).map(|i| i.entry).collect();
            self.actions.push(Action::Commit { slot, entries });
            for (id, barrier) in mine {
                self.forwarded.remove(&id);
                if let Some(at) = self.outstanding.remove(&id) {
                    self.observe_latency(self.now.0.saturating_sub(at.0));
                    self.actions.push(if barrier {
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

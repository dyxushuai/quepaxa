//! Proposer side: batching, forwarding to the leader, hedging, and driving slots.
//!
//! A replica that does not lead forwards its entries to the leader and proposes them
//! itself only if they are not committed within the hedge delay δ; commit-time dedup
//! makes the occasional double inclusion harmless. A slot someone else started is driven
//! by this replica after `(position + 1) · δ` without progress, so a crashed proposer
//! never leaves a gap. δ adapts to twice the smoothed commit latency, within
//! `[hedge_min, hedge_max]`. None of these timers affects safety.

use alloc::vec::Vec;

use super::{Active, LocalBatch, Replica};
use crate::batch::{self, Item};
use crate::codec::{Body, WireProposal};
use crate::durable::Record;
use crate::proposal::{LEADER_PRIORITY, Proposal, ValueKey};
use crate::proposer::{Progress, Reply, SlotProposer};
use crate::types::{MAX_STEP, NodeId, Slot, Timestamp};

impl Replica {
    pub(super) fn on_reply(
        &mut self,
        from: NodeId,
        slot: Slot,
        req_step: u32,
        step: u32,
        first: Option<WireProposal>,
        prev_agg: Option<WireProposal>,
    ) {
        // A recorder never reports a step below the request's; huge steps are malformed.
        if step < req_step || step > MAX_STEP {
            self.metrics.messages_rejected += 1;
            return;
        }
        let Some(active) = self.proposers.get_mut(&slot) else { return };
        if req_step != active.prop.step() {
            return;
        }
        let Some(request) = active.prop.request_for(from).cloned() else { return };
        let resolve = |w: Option<WireProposal>| w.map(|w| w.resolve(&request).ok_or(())).transpose();
        let (Ok(first), Ok(prev_agg)) = (resolve(first), resolve(prev_agg)) else {
            self.metrics.messages_rejected += 1;
            return;
        };
        match active.prop.on_reply(from, Reply { req_step, step, first, prev_agg }, &mut self.rng) {
            Progress::Waiting => {}
            Progress::Advanced => self.send_requests(slot, false),
            Progress::Decided(p) => {
                if req_step == 4 && p.priority == LEADER_PRIORITY {
                    self.metrics.fast_path_decisions += 1;
                }
                #[cfg(feature = "model")]
                self.decided_steps.insert(slot, (p.clone(), req_step));
                self.learn(slot, p, true);
            }
        }
    }

    /// Sends the current step's unanswered requests; `resend` skips the local recorder,
    /// whose delivery is reliable.
    pub(super) fn send_requests(&mut self, slot: Slot, resend: bool) {
        let me = self.me;
        let resend_at = self.now.after(self.cfg.retransmit_interval);
        let h_ready = self.reserved_durable.is_some_and(|t| slot.0 <= t);
        let Some(a) = self.proposers.get_mut(&slot) else { return };
        a.resend_at = resend_at;
        let step = a.prop.step();
        let reqs: Vec<(NodeId, Proposal)> =
            a.prop.requests().filter(|(j, _)| !(resend && *j == me)).map(|(j, p)| (j, p.clone())).collect();
        for (j, proposal) in reqs {
            // The leader's own top-priority proposal (only ever sent at the first step) may
            // leave only once its reservation is durable; later phases may forward it freely.
            let gate = step == 4 && proposal.priority == LEADER_PRIORITY && !h_ready && j != me;
            let body = Body::RecordRequest { slot, step, proposal };
            if gate {
                let out = self.encode(j, body);
                self.gate.hold(out);
            } else {
                self.send(j, body);
            }
        }
    }

    fn leads_now(&self) -> bool {
        self.schedule.leader(Slot(self.committed)) == self.me
    }

    /// The hedge delay δ.
    pub(super) fn hedge_delay(&self) -> u64 {
        let (lo, hi) = (self.cfg.hedge_min, self.cfg.hedge_max);
        self.commit_latency.map_or(hi / 4, |l| l.saturating_mul(2)).clamp(lo, hi)
    }

    /// A batch of entries the leader received from a follower.
    pub(super) fn on_forward(&mut self, bytes: &bytes::Bytes) {
        let Ok(entries) = batch::decode(bytes) else {
            self.metrics.messages_rejected += 1;
            return;
        };
        let cap =
            self.cfg.max_outstanding_proposals as usize * self.schedule.members(Slot(self.committed)).len();
        if !self.leads_now() || self.pending.len() + entries.len() > cap {
            return; // the origin hedges
        }
        self.pending.extend(entries);
    }

    /// Starts as many slots as the window allows, after handing entries to the leader if
    /// this replica does not lead.
    pub(super) fn drive(&mut self) {
        if self.fenced {
            return;
        }
        if !self.leads_now() {
            self.forward_pending();
        }
        // Batches whose entries all committed elsewhere are dead weight.
        let dedup = &self.dedup;
        self.batches
            .retain(|_, b| b.slot.is_some() || b.entries.iter().any(|&(o, i, id)| !dedup.contains(o, i, id)));
        while (self.proposers.len() as u64) < self.window() {
            let seq = match self.batches.iter().find(|(_, b)| b.slot.is_none() && !b.entries.is_empty()) {
                Some((seq, _)) => *seq,
                None => match self.cut_batch() {
                    Some(seq) => seq,
                    None => break,
                },
            };
            let Some(slot) = self.alloc_slot() else { break };
            self.start(slot, seq);
        }
    }

    fn forward_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let leader = self.schedule.leader(Slot(self.committed));
        let entries: Vec<Item> = self.pending.drain(..).collect();
        for i in
            entries.iter().filter(|i| i.entry.origin == self.me && i.entry.incarnation == self.incarnation)
        {
            self.forwarded.insert(i.entry.id, (i.clone(), self.now));
        }
        let mut chunk = batch::Builder::new(self.cfg.max_batch_bytes as usize);
        for e in entries {
            if !chunk.fits(&e) {
                self.send(leader, Body::Forward { batch: batch::encode(&chunk.take()) });
                self.metrics.forwarded += 1;
            }
            chunk.push(e);
        }
        self.send(leader, Body::Forward { batch: batch::encode(&chunk.take()) });
        self.metrics.forwarded += 1;
    }

    /// Cuts the next batch: hedged entries always, pending ones only while leading.
    fn cut_batch(&mut self) -> Option<u64> {
        let lead = self.leads_now();
        let mut entries = batch::Builder::new(self.cfg.max_batch_bytes as usize);
        loop {
            let queue = if !self.hedged.is_empty() {
                &mut self.hedged
            } else if lead && !self.pending.is_empty() {
                &mut self.pending
            } else {
                break;
            };
            let Some(e) = queue.front() else { break };
            if !entries.fits(e) {
                break;
            }
            let Some(e) = queue.pop_front() else { break };
            let (o, inc, id) = e.key();
            if self.dedup.contains(o, inc, id) {
                continue;
            }
            entries.push(e);
        }
        (!entries.is_empty()).then(|| self.new_batch(entries.take()))
    }

    fn new_batch(&mut self, entries: Vec<Item>) -> u64 {
        let seq = self.next_batch;
        self.next_batch += 1;
        let key = ValueKey { origin: self.me, incarnation: self.incarnation, seq };
        let ids = entries.iter().map(Item::key).collect();
        self.batches
            .insert(seq, LocalBatch { key, value: batch::encode(&entries), entries: ids, slot: None });
        seq
    }

    fn alloc_slot(&mut self) -> Option<Slot> {
        let end = self.committed + self.window();
        let mut s = self.next_alloc.max(self.committed);
        while s < end {
            let slot = Slot(s);
            if !self.log.contains_key(&slot)
                && !self.proposers.contains_key(&slot)
                && !self.registers.contains_key(&slot)
            {
                self.next_alloc = s + 1;
                return Some(slot);
            }
            s += 1;
        }
        None
    }

    fn start(&mut self, slot: Slot, seq: u64) {
        let Some(b) = self.batches.get_mut(&seq) else { return };
        b.slot = Some(slot);
        let proposal = Proposal { priority: 0, key: b.key, value: b.value.clone() };
        let leader = self.schedule.leader(slot) == self.me && slot.0 >= self.h_floor;
        if leader && self.reserved_through.is_none_or(|t| slot.0 > t) {
            let through = slot.0 + 4 * self.window();
            self.reserved_through = Some(through);
            self.gate.write(Record::Reservation { through: Slot(through) }.encode());
            self.after_durable(super::Effect::ReservationDurable(through));
        }
        // The slot's epoch is fixed: it was decided at least `LAG` slots before any slot we drive.
        let recorders = self.schedule.members(slot).to_vec();
        let prop = SlotProposer::new(recorders, leader, proposal, &mut self.rng);
        self.proposers.insert(slot, Active { prop, batch: seq, resend_at: self.now });
        self.send_requests(slot, false);
    }

    /// Undecided slots in the window someone else drives, with when to take them over.
    fn stalled(&self) -> impl Iterator<Item = (Slot, Timestamp)> + '_ {
        let delta = self.hedge_delay();
        let end = Slot(self.committed + self.window());
        self.seen
            .range(..end)
            .filter(|(s, _)| !self.log.contains_key(s) && !self.proposers.contains_key(s))
            .map(move |(s, t)| {
                let wait = (self.schedule.position(self.me, *s) + 1).saturating_mul(delta);
                (*s, t.after(wait))
            })
    }

    pub(super) fn earliest_timer(&self) -> Option<Timestamp> {
        let resend = self.proposers.values().map(|a| a.resend_at);
        let stall = self.stalled().map(|(_, at)| at);
        let hedge = self.forwarded.values().map(|(_, at)| *at).min().map(|t| t.after(self.hedge_delay()));
        // Behind a peer: retry bulk catch-up on the timer, not only when a message arrives.
        let behind = self.peer_committed > self.committed + 1
            && self.catch_up.or(self.ahead_peer.map(|p| (p, self.now))).is_some();
        let catch_up =
            behind.then(|| self.catch_up.map_or(self.now, |(_, t)| t.after(self.cfg.retransmit_interval)));
        resend
            .chain(stall)
            .chain(hedge)
            .chain(catch_up)
            .chain(self.retire_probe)
            .chain(self.startup_probe)
            .min()
    }

    pub(super) fn fire_timers(&mut self) {
        let now = self.now;
        if self.startup_probe.is_some_and(|t| t <= now) {
            self.probe_peers();
        }
        let due: Vec<Slot> =
            self.proposers.iter().filter(|(_, a)| a.resend_at <= now).map(|(s, _)| *s).collect();
        for slot in due {
            self.metrics.retransmits += 1;
            self.send_requests(slot, true);
        }
        let delta = self.hedge_delay();
        let late: Vec<_> =
            self.forwarded.iter().filter(|(_, (_, at))| at.after(delta) <= now).map(|(id, _)| *id).collect();
        for id in late {
            if let Some((e, _)) = self.forwarded.remove(&id) {
                self.metrics.hedged += 1;
                self.hedged.push_back(e);
            }
        }
        if self.peer_committed > self.committed + 1 && self.catch_up_due() {
            // Prefer the most advanced peer; the previous target may be the one that failed.
            let peer = self.ahead_peer.or(self.catch_up.map(|(p, _)| p));
            if let Some(peer) = peer {
                self.metrics.catch_up_retries += 1;
                self.request_catch_up(peer);
            }
        }
        let stalled: Vec<Slot> = self.stalled().filter(|(_, at)| *at <= now).map(|(s, _)| s).collect();
        for slot in stalled {
            let seq = self.new_batch(Vec::new());
            self.start(slot, seq);
        }
    }
}

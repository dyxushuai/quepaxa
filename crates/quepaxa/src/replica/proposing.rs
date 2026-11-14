//! Proposer side: batching, forwarding to the leader, hedging, and driving slots.
//!
//! A replica that does not lead forwards its entries to the leader. If they are not
//! committed within the hedge delay δ (sooner if the leader is silent), it suspects the
//! leader and hands them to the next member instead, or proposes them itself when that is
//! this replica (`watch.rs`); commit-time dedup makes the occasional double inclusion
//! harmless. A slot someone else started is driven
//! by this replica after `(position + 1) · δ` without progress, so a crashed proposer
//! never leaves a gap. δ adapts to four times the smoothed commit latency of entries that
//! were not hedged, within `[hedge_min, hedge_max]`. None of these timers affects safety.

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

    pub(super) fn leads_now(&self) -> bool {
        self.schedule.leader(Slot(self.committed)) == self.me
    }

    /// The hedge delay δ.
    pub(super) fn hedge_delay(&self) -> u64 {
        let (lo, hi) = (self.cfg.hedge_min, self.cfg.hedge_max);
        self.commit_latency.map_or(hi / 4, |l| l.saturating_mul(4)).clamp(lo, hi)
    }

    /// A batch of entries a follower handed to this replica: as the leader, or as the deputy
    /// of a leader that follower suspects (`watch.rs`), which proposes them itself.
    pub(super) fn on_forward(&mut self, from: NodeId, bytes: &bytes::Bytes) {
        let Ok(entries) = batch::decode(bytes) else {
            self.metrics.messages_rejected += 1;
            return;
        };
        #[cfg(feature = "model")]
        {
            let own = entries
                .iter()
                .filter(|i| i.entry.origin == self.me && i.entry.incarnation == self.incarnation);
            self.probes.d3 += own.count() as u64;
        }
        let cap =
            self.cfg.max_outstanding_proposals as usize * self.schedule.members(Slot(self.committed)).len();
        // Stepped aside, this replica hands them on (`watch.rs`); so it does for a follower
        // that has not yet seen this replica stop leading, whose entries belong to the new
        // leader, not to a deputy.
        let lagging =
            self.watch.committed.get(&from).is_some_and(|&c| self.schedule.leader(Slot(c)) == self.me);
        let queue = if self.leads_now() || self.watch.aside || lagging {
            &mut self.pending
        } else {
            &mut self.relayed
        };
        if queue.len() + entries.len() > cap {
            return; // the origin hedges
        }
        queue.extend(entries);
        self.ack_forward(from);
    }

    /// Starts as many slots as the window allows, after handing entries to the target if
    /// this replica does not propose them itself.
    pub(super) fn drive(&mut self) {
        if self.fenced {
            return;
        }
        self.check_aside();
        // Fixed for this call: starting slots changes neither suspicions nor the disk.
        let direct = self.proposes_directly();
        if !direct {
            self.forward_pending();
        }
        // Batches whose entries all committed elsewhere are dead weight.
        let dedup = &self.dedup;
        self.batches
            .retain(|_, b| b.slot.is_some() || b.entries.iter().any(|&(o, i, id)| !dedup.contains(o, i, id)));
        // Hedging around a target that still talks: one slot at a time, or the hedges contend
        // with its slots and stretch a slowdown they were meant to cover. Entries other
        // followers handed here (as their deputy) are not hedges.
        let deputy = || {
            self.relayed.iter().any(|i| i.entry.origin != self.me)
                || self.batches.values().any(|b| b.entries.iter().any(|&(o, _, _)| o != self.me))
        };
        let cap = if direct || deputy() { self.window() } else { 1 };
        while (self.proposers.len() as u64) < cap {
            let waiting = self.batches.iter().find(|(_, b)| b.slot.is_none() && !b.entries.is_empty());
            // Nothing to propose: skip the search for a slot, which probes the log per slot
            // and ran on every input.
            if waiting.is_none() && self.relayed.is_empty() && !(direct && !self.pending.is_empty()) {
                break;
            }
            let waiting = waiting.map(|(seq, _)| *seq);
            // A batch is cut only for a slot it can take: one cut and left waiting would be
            // proposed later in someone else's slots.
            let Some(slot) = self.free_slot() else { break };
            let seq = match waiting {
                Some(seq) => seq,
                None => match self.cut_batch(direct) {
                    Some(seq) => seq,
                    None => break,
                },
            };
            self.next_alloc = slot.0 + 1;
            self.start(slot, seq);
        }
    }

    fn forward_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let leader = self.target();
        self.forwarded_now();
        let entries: Vec<Item> = self.pending.drain(..).collect();
        for i in
            entries.iter().filter(|i| i.entry.origin == self.me && i.entry.incarnation == self.incarnation)
        {
            let _earlier = self.forwarded.insert(i.entry.id, (i.clone(), self.now));
            #[cfg(feature = "model")]
            if _earlier.is_some_and(|(_, at)| at < self.now) {
                self.probes.d2_restarts += 1;
            }
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

    /// No longer the leader (an order change took effect): the entries of batches not yet in
    /// a slot go to the new leader, instead of being proposed later in its slots.
    pub(super) fn hand_on_unstarted(&mut self) {
        let mut handed = Vec::new();
        self.batches.retain(|_, b| {
            if b.slot.is_none() {
                // Our own encoding: it decodes.
                handed.extend(batch::decode(&b.value).unwrap_or_default());
            }
            b.slot.is_some()
        });
        let dedup = &self.dedup;
        self.pending.extend(handed.into_iter().filter(|e| {
            let (o, inc, id) = e.key();
            !dedup.contains(o, inc, id)
        }));
    }

    /// A local batch lost its slot. The leader, or a follower that suspects it (`watch.rs`),
    /// proposes it again in another slot; any other follower hands its own entries back to the
    /// leader, since a follower's proposal loses to a live leader's in slot after slot, and
    /// hedges them again only if the leader stays silent. Entries another follower handed
    /// here are proposed again here: that follower judged the leader, this one may not have.
    /// Not when the slot went to the target's own value and the target has decided a slot
    /// itself since the batch was proposed (`watch.progressed`): it is alive, and handed them
    /// here only by stepping aside for a moment; proposed here they would lose to it slot
    /// after slot as well.
    pub(super) fn lost_slot(&mut self, seq: u64, winner: NodeId, started: Timestamp) {
        if self.proposes_directly() {
            if let Some(b) = self.batches.get_mut(&seq) {
                b.slot = None;
            }
            return;
        }
        let Some(b) = self.batches.remove(&seq) else { return };
        let deciding = winner == self.target() && self.watch.progressed > started;
        // Our own encoding: it decodes.
        let entries = batch::decode(&b.value).unwrap_or_default();
        for e in entries {
            let (o, inc, id) = e.key();
            if self.dedup.contains(o, inc, id) {
                continue;
            }
            #[cfg(feature = "model")]
            if o == self.me
                && inc == self.incarnation
                && (self.forwarded.contains_key(&id) || self.pending.iter().any(|i| i.key() == (o, inc, id)))
            {
                self.probes.d2 += 1;
            }
            if o == self.me || deciding { self.pending.push_back(e) } else { self.relayed.push_back(e) }
        }
    }

    /// Cuts the next batch: relayed entries always, own pending ones only when proposing them
    /// directly.
    fn cut_batch(&mut self, lead: bool) -> Option<u64> {
        let mut entries = batch::Builder::new(self.cfg.max_batch_bytes as usize);
        loop {
            let queue = if !self.relayed.is_empty() {
                &mut self.relayed
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

    /// The next slot in the window this replica may start. The leader proposing its own and
    /// forwarded entries stops at the first slot another member leads (an order change
    /// taking effect): proposed there, they would decide in the new leader's slots, and
    /// miss-based rotation would count them against it. Past that slot it is no longer the
    /// leader and hands the entries on.
    fn free_slot(&self) -> Option<Slot> {
        let as_leader = self.leads_now() && !self.watch.aside;
        let end = self.committed + self.window();
        let mut s = self.next_alloc.max(self.committed);
        while s < end {
            let slot = Slot(s);
            if as_leader && self.schedule.leader(slot) != self.me {
                return None;
            }
            if !self.log.contains_key(&slot)
                && !self.proposers.contains_key(&slot)
                && !self.registers.contains_key(&slot)
            {
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
        self.proposers.insert(slot, Active { prop, batch: seq, resend_at: self.now, started: self.now });
        self.send_requests(slot, false);
    }

    /// Undecided slots in the window someone else drives, with when to take them over.
    fn stalled(&self) -> impl Iterator<Item = (Slot, Timestamp)> + '_ {
        let end = Slot(self.committed + self.window());
        self.seen
            .range(..end)
            .filter(|(s, _)| !self.log.contains_key(s) && !self.proposers.contains_key(s))
            .map(|(s, t)| (*s, self.takeover_at(*s, *t)))
    }

    pub(super) fn earliest_timer(&self) -> Option<Timestamp> {
        let resend = self.proposers.values().map(|a| a.resend_at);
        let stall = self.stalled().map(|(_, at)| at);
        let hedge = self.forwarded.values().filter_map(|(_, at)| self.hedge_at(*at)).min();
        // Behind a peer: retry bulk catch-up on the timer, not only when a message arrives.
        let behind = self.peer_committed > self.committed + 1
            && self.catch_up.or(self.ahead_peer.map(|p| (p, self.now))).is_some();
        let catch_up =
            behind.then(|| self.catch_up.map_or(self.now, |(_, t)| t.after(self.cfg.retransmit_interval)));
        resend
            .chain(stall)
            .chain(hedge)
            .chain(catch_up)
            .chain(self.retirement.probe_at())
            .chain(self.reads.resend_at())
            .chain(self.startup_probe)
            .chain(self.watch.ack_due.values().copied())
            .chain(self.aside_at().filter(|_| !self.watch.aside))
            .chain(self.placement.next_probe)
            .min()
    }

    pub(super) fn fire_timers(&mut self) {
        self.resend_reads();
        self.check_aside();
        let now = self.now;
        let ack_due: Vec<_> =
            self.watch.ack_due.iter().filter(|(_, at)| **at <= now).map(|(node, _)| *node).collect();
        for node in ack_due {
            self.ack_forward(node);
        }
        if self.startup_probe.is_some_and(|t| t <= now) {
            self.probe_peers();
        }
        self.fire_probe();
        let due: Vec<Slot> =
            self.proposers.iter().filter(|(_, a)| a.resend_at <= now).map(|(s, _)| *s).collect();
        for slot in due {
            self.metrics.retransmits += 1;
            self.send_requests(slot, true);
        }
        if self.forwarded.values().any(|(_, at)| self.silent_at(*at).is_some_and(|t| t <= now)) {
            self.suspect_target();
        }
        // The target talks but is slow: entries it left uncommitted too long are proposed here;
        // while stepped aside (this replica's own slots are stuck) handed to the target again,
        // in case the forward was lost or refused.
        let aside = self.watch.aside;
        let late: Vec<_> = self
            .forwarded
            .iter()
            .filter(|(_, (_, at))| self.late_at(*at).is_some_and(|t| t <= now))
            .map(|(id, _)| *id)
            .collect();
        for id in late {
            if let Some((e, _)) = self.forwarded.remove(&id) {
                if aside {
                    self.pending.push_back(e);
                    continue;
                }
                if let Some(o) = self.outstanding.get_mut(&id) {
                    o.2 = true;
                }
                self.metrics.hedged += 1;
                self.relayed.push_back(e);
            }
        }
        if self.peer_committed > self.committed + 1 && self.catch_up_due() {
            // Prefer the most advanced peer; the previous target may be the one that failed.
            let peer = self.ahead_peer.or(self.catch_up.map(|(p, _)| p));
            if let Some(peer) = peer {
                self.metrics.catch_up_retries += 1;
                self.request_catch_up(peer);
            }
        } else if self.catch_up_due()
            && let Some(peer) = self.read_catch_up_from()
        {
            // The answers' commit indexes may predate the slots their marks wait for, and the
            // round's resend timer brings this back here until the read is ready.
            self.metrics.catch_up_retries += 1;
            self.request_catch_up(peer);
        }
        let stalled: Vec<Slot> = self.stalled().filter(|(_, at)| *at <= now).map(|(s, _)| s).collect();
        for slot in stalled {
            let seq = self.new_batch(Vec::new());
            self.start(slot, seq);
        }
    }
}

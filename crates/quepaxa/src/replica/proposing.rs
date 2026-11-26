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

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use super::{Active, Replica};
use crate::batch::{self, Kind, Piece};
use crate::codec::{Body, WireProposal};
use crate::content::PinError;
use crate::durable::Record;
use crate::proposal::{LEADER_PRIORITY, Proposal};
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
        let (sent_at, resent) = (active.sent_at, active.prop.was_resent());
        let Some(request) = active.prop.request_for(from) else { return };
        let req_key = request.key;
        let resolve = |w: Option<WireProposal>| w.map(|w| w.resolve(request).ok_or(())).transpose();
        let (Ok(first), Ok(prev_agg)) = (resolve(first), resolve(prev_agg)) else {
            self.metrics.messages_rejected += 1;
            return;
        };
        for p in first.iter().chain(prev_agg.iter()) {
            // Only record foreign candidate holders: the proposer already holds its own
            // working request's content (rule P), so the uncontended path never allocates.
            if p.key != req_key {
                active.holders.insert(p.key, from);
            }
        }
        match active.prop.on_reply(from, Reply { req_step, step, first, prev_agg }, &mut self.rng) {
            Progress::Waiting => {}
            Progress::Advanced => {
                let adopted = active.prop.working().key;
                active.holder = active.holders.get(&adopted).copied();
                active.holders.clear();
                self.send_requests(slot, false);
            }
            Progress::Decided(p) => {
                if req_step == 4 && p.priority == LEADER_PRIORITY {
                    self.metrics.fast_path_decisions += 1;
                }
                // The first step is the slot's first: never resent, the decision answers the
                // one copy of each request, so the sample is a true round trip; if resent,
                // the time since the last copy is a lower bound that only raises the estimate.
                if req_step == 4 {
                    self.note_request_rtt(self.now.0.saturating_sub(sent_at.0), resent);
                }
                #[cfg(feature = "model")]
                self.decided_steps.insert(slot, (p.clone(), req_step));
                self.learn(slot, p, true, Some(from));
            }
        }
    }

    /// Rises at once to a slower sample and decays by an eighth toward a faster one, as R
    /// does (`watch.rs`): a link that starts queueing is followed at once. A resent slot's
    /// decision may answer either copy, so the time since the last copy only raises the
    /// estimate (or sets it if it already exceeds half the floor), never pulls it down:
    /// otherwise a link whose round trip crosses `retransmit_interval` resends every slot
    /// and never gets a clean sample to raise the floor (#231).
    fn note_request_rtt(&mut self, sample: u64, resent: bool) {
        let sample = sample.min(self.cfg.hedge_max);
        match self.request_rtt {
            Some(avg) if sample >= avg => self.request_rtt = Some(sample),
            Some(avg) if !resent => self.request_rtt = Some(avg - avg / 8 + sample / 8),
            Some(_) => {}
            None if !resent || sample.saturating_mul(2) > self.cfg.retransmit_interval => {
                self.request_rtt = Some(sample);
            }
            None => {}
        }
    }

    /// How long to wait for an answer before sending again, after `resent` earlier resends
    /// (`None`: a first send). Each resend waits twice as long as the one before, from the
    /// floor up to `hedge_max` (or the floor, if that is longer) (#231). The floor is
    /// `retransmit_interval`, or twice the smoothed first-step round trip (`request_rtt`) if
    /// that is longer. On a saturated link an unanswered message is queued, not lost: resent
    /// at a fixed period shorter than the queue, every request went out again with its value,
    /// the queue grew, decisions collapsed and the leader rotated. The floor follows the round
    /// trip, so nothing is resent while its answer can still be on the way.
    pub(super) fn resend_wait(&self, resent: Option<u32>) -> u64 {
        let retransmit = self.cfg.retransmit_interval;
        let floor = self.request_rtt.map_or(retransmit, |rtt| rtt.saturating_mul(2).max(retransmit));
        let wait = resent.map_or(floor, |k| floor.saturating_mul(1 << (k.min(31) + 1)));
        wait.min(self.cfg.hedge_max.max(floor))
    }

    /// Sends the current step's unanswered requests; `resend` skips the local recorder,
    /// whose delivery is reliable. Resends back off (`resend_wait`); a new step starts from
    /// the floor again. Rule P (#172): holds back record requests until this replica holds
    /// every data batch referenced by the proposer's working value.
    pub(super) fn send_requests(&mut self, slot: Slot, resend: bool) {
        let Some((refs, holder)) = self.proposers.get(&slot).map(|a| {
            let w = a.prop.working();
            (batch::refs(&w.value), a.holder.unwrap_or(w.key.origin))
        }) else {
            return;
        };
        if !refs.is_empty() {
            match self.pin_ref_list_for_proposer(&refs, slot) {
                Err(()) => {
                    if let Some(a) = self.proposers.remove(&slot) {
                        self.release_value_refs(&a.prop.working().value);
                    }
                    return;
                }
                Ok(missing) if !missing.is_empty() => {
                    if let Some(a) = self.proposers.get_mut(&slot) {
                        a.waiting_content = true;
                    }
                    self.fetch_refs(slot, holder, &missing);
                    return;
                }
                Ok(_) => {}
            }
        }
        let (me, now) = (self.me, self.now);
        let h_ready = self.reservation.covers(slot);
        let Some(a) = self.proposers.get_mut(&slot) else { return };
        let resent = resend.then(|| a.prop.resent());
        let wait = self.resend_wait(resent);
        let Some(a) = self.proposers.get_mut(&slot) else { return };
        a.waiting_content = false;
        a.sent_at = now;
        a.resend_at = now.after(wait);
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

    /// A batch of entries (or references) a follower handed to this replica: as the leader,
    /// or as the deputy of a leader that follower suspects (`watch.rs`), which proposes them
    /// itself.
    ///
    /// Every forward taken is acknowledged by its number (`forwards.rs`). Inline entries
    /// already committed are dropped: a forward sent again may carry some.
    pub(super) fn on_forward(&mut self, from: NodeId, seq: u64, bytes: &bytes::Bytes) {
        let Ok(mut pieces) = batch::decode_pieces(bytes) else {
            self.metrics.messages_rejected += 1;
            return;
        };
        let mut refs = Vec::new();
        let mut missing = Vec::new();
        for p in &pieces {
            if let Piece::Ref(r) = *p {
                match self.content.check(r) {
                    Ok(()) => {}
                    Err(PinError::Missing) => missing.push(r),
                    Err(PinError::Mismatch) => {
                        self.metrics.messages_rejected += 1;
                        return;
                    }
                }
            }
        }
        pieces.retain(|p| match *p {
            Piece::Entry(ref i) => {
                let (o, inc, id) = i.key();
                !self.dedup.contains(o, inc, id)
            }
            Piece::Ref(r) => {
                if Self::ref_all_committed(&self.content, &self.dedup, r) {
                    false
                } else {
                    if self.content.check(r).is_ok() {
                        refs.push(r.id);
                    }
                    true
                }
            }
        });
        // Stepped aside, this replica hands them on (`watch.rs`); so it does for a follower
        // that has not yet seen this replica stop leading, whose entries belong to the new
        // leader, not to a deputy.
        let lagging = self
            .watch
            .committed
            .get(&from)
            .is_some_and(|&c| self.schedule.routing_order(Slot(c))[0] == self.me);
        let to_pending = self.proposes_directly() || self.watch.aside || lagging;
        if !missing.is_empty() {
            // Do not let a forward referencing missing data enter `entries` (if its origin
            // crashed before anyone received `Data`, it could otherwise poison a local batch
            // and wedge a slot). Stage it briefly while fetching from `from`.
            self.stage_forward(from, seq, to_pending, pieces, &missing);
            return;
        }
        let cap =
            self.cfg.max_outstanding_proposals as usize * self.schedule.members(Slot(self.committed)).len();
        if self.entries.received(pieces, to_pending, cap) {
            for id in refs {
                self.content.keep(id);
            }
            self.watch.fwd.took(from, seq);
            self.ack_forward(from);
        }
    }

    /// Starts as many slots as the window allows, after handing entries to the target if
    /// this replica does not propose them itself.
    pub(super) fn drive(&mut self) {
        if self.phase.fenced() {
            return;
        }
        self.check_aside();
        // Fixed for this call: starting slots changes neither suspicions nor the disk.
        let direct = self.proposes_directly();
        self.forward_pending(direct);
        // Batches whose entries all committed elsewhere are dead weight.
        let pruned_refs = {
            let (content, dedup) = (&self.content, &self.dedup);
            self.entries.prune(dedup, |r| Self::ref_all_committed(content, dedup, r))
        };
        for id in pruned_refs {
            self.maybe_release_content(id);
        }
        // Hedging around a target that still talks: one slot at a time, or the hedges contend
        // with its slots and stretch a slowdown they were meant to cover. Entries other
        // followers handed here (as their deputy) are not hedges.
        let cap = if direct || self.entries.deputy() { self.active_window() } else { 1 };
        while (self.proposers.len() as u64) < cap {
            // Nothing to propose: skip the search for a slot, which probes the log per slot
            // and ran on every input.
            if !self.entries.proposable(direct) {
                break;
            }
            // A batch is cut only for a slot it can take: one cut and left waiting would be
            // proposed later in someone else's slots.
            let Some(slot) = self.free_slot() else { break };
            let max = self.cfg.max_batch_bytes as usize;
            let seq = match self.entries.waiting() {
                Some(seq) => seq,
                None => {
                    let (cut_seq, skipped_refs) = {
                        let (content, dedup) = (&self.content, &self.dedup);
                        self.entries.cut(direct, max, dedup, |r| Self::ref_all_committed(content, dedup, r))
                    };
                    for id in skipped_refs {
                        self.maybe_release_content(id);
                    }
                    match cut_seq {
                        Some(seq) => seq,
                        None => break,
                    }
                }
            };
            self.next_alloc = slot.0 + 1;
            self.start(slot, seq);
        }
    }

    /// Hands what is pending to the target, unless this replica proposes it `direct`ly.
    /// Application chunks larger than `inline_batch_bytes` go to every member as `Data`, and
    /// travel in `Forward` as compact `Ref` items; control entries stay inline.
    fn forward_pending(&mut self, direct: bool) {
        if !direct
            && self.forward_flush_at().is_some()
            && !self.entries.pending_urgent(self.cfg.max_batch_bytes as usize)
        {
            return;
        }
        let pieces = self.entries.route(direct, self.now);
        if pieces.is_empty() {
            return;
        }
        let routed_refs: Vec<_> = pieces
            .iter()
            .filter_map(|p| match *p {
                Piece::Ref(r) => Some(r.id),
                Piece::Entry(_) => None,
            })
            .collect();
        let leader = self.target();
        self.watch.last_forward = Some((leader, self.now));
        self.forwarded_now();
        let (me, inc, max) = (self.me, self.incarnation, self.cfg.max_batch_bytes as usize);
        let own_ids = |ps: &[Piece]| -> Vec<_> {
            ps.iter()
                .filter_map(|p| match p {
                    Piece::Entry(i) if i.entry.origin == me && i.entry.incarnation == inc => Some(i.entry.id),
                    _ => None,
                })
                .collect()
        };
        let mut chunk = batch::PieceBuilder::new(max);
        let flush_inline = |this: &mut Self, chunk: &mut batch::PieceBuilder| {
            if !chunk.is_empty() {
                let ps = chunk.take();
                let ids = own_ids(&ps);
                this.send_forward(leader, ps, ids, Vec::new());
            }
        };
        if self.cfg.inline_batch_bytes >= self.cfg.max_batch_bytes {
            for p in pieces {
                if !chunk.fits(&p) {
                    flush_inline(self, &mut chunk);
                }
                chunk.push(p);
            }
        } else {
            let mut apps = batch::Builder::new(max);
            let flush_apps = |this: &mut Self, apps: &mut batch::Builder, chunk: &mut batch::PieceBuilder| {
                if apps.is_empty() {
                    return;
                }
                let items = apps.take();
                let ids: Vec<_> = items
                    .iter()
                    .filter(|i| i.entry.origin == me && i.entry.incarnation == inc)
                    .map(|i| i.entry.id)
                    .collect();
                let disseminated = this.disseminate_chunk(items);
                if let [Piece::Ref(r)] = disseminated.as_slice() {
                    flush_inline(this, chunk);
                    let r = *r;
                    this.send_forward(leader, disseminated, ids, alloc::vec![r]);
                } else {
                    for p in disseminated {
                        if !chunk.fits(&p) {
                            flush_inline(this, chunk);
                        }
                        chunk.push(p);
                    }
                }
            };
            for p in pieces {
                match p {
                    Piece::Entry(e) if e.kind == Kind::App => {
                        if !apps.fits(&e) {
                            flush_apps(self, &mut apps, &mut chunk);
                        }
                        apps.push(e);
                    }
                    other => {
                        flush_apps(self, &mut apps, &mut chunk);
                        if !chunk.fits(&other) {
                            flush_inline(self, &mut chunk);
                        }
                        chunk.push(other);
                    }
                }
            }
            flush_apps(self, &mut apps, &mut chunk);
        }
        flush_inline(self, &mut chunk);
        for id in routed_refs {
            self.maybe_release_content(id);
        }
    }

    /// No longer the leader (an order change took effect): the entries of batches not yet in
    /// a slot go to the new leader, instead of being proposed later in its slots.
    pub(super) fn hand_on_unstarted(&mut self) {
        self.entries.lost_lead(&self.dedup);
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
            self.entries.lost_slot_kept(seq);
            return;
        }
        let deciding = winner == self.target() && self.watch.progressed > started;
        let refs = self.entries.lost_slot(seq, deciding, &self.dedup);
        for r in refs {
            self.maybe_release_content(r.id);
        }
    }

    /// The next slot in the window this replica may start. The leader proposing its own and
    /// forwarded entries stops at the first slot another member leads (an order change
    /// taking effect): proposed there, they would decide in the new leader's slots, and
    /// miss-based rotation would count them against it. Past that slot it is no longer the
    /// leader and hands the entries on.
    fn free_slot(&self) -> Option<Slot> {
        let as_leader = self.leads_now() && self.proposes_directly();
        let end = self.committed + self.active_window();
        // Skip slots already decided by a peer: proposing there loses the slot, re-queues the
        // batch, and blasts RecordRequests across the whole handover window during catch-up.
        let mut s = self.next_alloc.max(self.committed).max(self.catch_up.ahead());
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
        let Some((key, value)) = self.entries.start(seq, slot) else { return };
        let proposal = Proposal { priority: 0, key, value };
        let leader = self.schedule.leader(slot) == self.me && self.reservation.may_lead(slot);
        if leader && let Some(through) = self.reservation.extend(slot, 4 * self.window()) {
            self.gate.write(Record::Reservation { through: Slot(through) }.encode());
            self.after_durable(super::Effect::ReservationDurable(through));
        }
        // The slot's epoch is fixed: it was decided at least `LAG` slots before any slot we drive.
        let recorders = self.schedule.members(slot).to_vec();
        let prop = SlotProposer::new(recorders, leader, proposal, &mut self.rng);
        self.proposers.insert(
            slot,
            Active {
                prop,
                batch: seq,
                resend_at: self.now,
                sent_at: self.now,
                started: self.now,
                waiting_content: false,
                holder: None,
                holders: BTreeMap::new(),
            },
        );
        self.send_requests(slot, false);
    }

    /// Undecided slots in the window someone else drives, with when to take them over.
    fn stalled(&self) -> impl Iterator<Item = (Slot, Timestamp)> + '_ {
        let end = Slot(self.committed + self.active_window());
        self.seen
            .range(..end)
            .filter(|(s, _)| !self.log.contains_key(s) && !self.proposers.contains_key(s))
            .map(|(s, t)| (*s, self.takeover_at(*s, *t)))
    }

    pub(super) fn earliest_timer(&self) -> Option<Timestamp> {
        let resend = self.proposers.values().filter(|a| !a.waiting_content).map(|a| a.resend_at);
        let stall = self.stalled().map(|(_, at)| at);
        let hedge = self.entries.forwarded_since().filter_map(|(id, at)| self.hedge_at(id, at)).min();
        // Behind a peer: retry bulk catch-up on the timer, not only when a message arrives.
        let catch_up = self.catch_up.deadline(self.committed, self.now, self.cfg.retransmit_interval);
        resend
            .chain(stall)
            .chain(hedge)
            .chain(catch_up)
            .chain(self.retirement.probe_at())
            .chain(self.reads.resend_at())
            .chain(self.phase.probe_at())
            .chain(self.watch.ack_due.values().copied())
            .chain(self.forward_flush_at())
            .chain(self.watch.fwd.due())
            .chain(self.aside_at().filter(|_| !self.watch.aside))
            .chain(self.placement.next_probe)
            .chain(self.earliest_unresolved())
            .chain(self.earliest_fetch())
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
        if self.phase.probe_at().is_some_and(|t| t <= now) {
            self.probe_peers();
        }
        self.fire_probe();
        self.retry_unresolved();
        self.retry_fetches();
        let due: Vec<Slot> = self
            .proposers
            .iter()
            .filter(|(_, a)| !a.waiting_content && a.resend_at <= now)
            .map(|(s, _)| *s)
            .collect();
        for slot in due {
            self.metrics.retransmits += 1;
            self.send_requests(slot, true);
        }
        self.resend_forwards();
        if self.entries.forwarded_since().any(|(_, at)| self.silent_at(at).is_some_and(|t| t <= now)) {
            self.suspect_target();
        }
        // The target talks but is slow: entries it left uncommitted too long are proposed here;
        // while stepped aside (this replica's own slots are stuck) handed to the target again,
        // in case the forward was lost or refused.
        let aside = self.watch.aside;
        let late: Vec<_> = self
            .entries
            .forwarded_since()
            .filter_map(|(id, at)| self.lateness(id, at).filter(|(t, _)| *t <= now).map(|(_, b)| (id, b)))
            .collect();
        if !aside {
            self.metrics.hedges_at_bound += late.iter().filter(|(_, bound)| *bound).count() as u64;
        }
        self.metrics.hedged += self.entries.late(late.into_iter().map(|(id, _)| id).collect(), aside);
        // Outside the membership, the answers' commit indexes may predate the slots the read
        // marks wait for, and the round's resend timer brings this back here until the read
        // is ready.
        let interval = self.cfg.retransmit_interval;
        if let Some(peer) = self.catch_up.retry(self.committed, now, interval, || self.read_catch_up_from()) {
            self.metrics.catch_up_retries += 1;
            self.request_catch_up(peer);
        }
        let stalled: Vec<Slot> = self.stalled().filter(|(_, at)| *at <= now).map(|(s, _)| s).collect();
        for slot in stalled {
            let seq = self.entries.new_batch(Vec::new());
            self.start(slot, seq);
        }
    }
}

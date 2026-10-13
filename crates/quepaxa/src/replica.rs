//! The sans-IO replica: one proposer and one recorder per slot, a decided log, and the
//! durability gate that keeps replies behind the writes they depend on (ADR 0002).

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;
use bytes::Bytes;
use core::mem;

use crate::api::{Action, Config, DropReason, Error, Input, Metrics, Outbound};
use crate::batch::{self, Entry};
use crate::codec::{Body, Envelope, Message, WireCtx, WireProposal, reply_proposal};
use crate::durable::Record;
use crate::proposal::{LEADER_PRIORITY, Proposal, ValueKey};
use crate::proposer::{Progress, Reply, SlotProposer};
use crate::recorder::Register;
use crate::rng::ChaCha20;
use crate::types::{NodeId, PersistId, ProposalId, Slot, Timestamp};

struct Decision {
    proposal: Proposal,
    durable: bool,
}

struct Active {
    prop: SlotProposer,
    batch: u64,
    resend_at: Timestamp,
}

/// A local batch: proposed in at most one slot at a time until it is decided.
struct LocalBatch {
    key: ValueKey,
    value: Bytes,
    ids: Vec<ProposalId>,
    slot: Option<Slot>,
}

/// Work deferred until the next `Persist` is acknowledged.
enum Effect {
    Ready,
    ReservationDurable(u64),
    DecidedDurable(Slot),
    LocalReply(Body),
}

struct InFlight {
    id: u64,
    effects: Vec<Effect>,
    parked: Vec<Outbound>,
}

/// One member of a QuePaxa cluster. See the crate docs for the driving contract.
pub struct Replica {
    cfg: Config,
    me: NodeId,
    leader: NodeId,
    wire: WireCtx,
    rng: ChaCha20,
    incarnation: u64,
    now: Timestamp,
    fenced: bool,
    metrics: Metrics,

    // Durability gate.
    ready: bool,
    next_persist: u64,
    acked: Option<u64>,
    writes: Vec<Bytes>,
    gated: Vec<Outbound>,
    effects: Vec<Effect>,
    /// Issued persists awaiting acknowledgement, with the effects and sends parked behind each.
    in_flight: VecDeque<InFlight>,
    actions: Vec<Action>,
    inbox: VecDeque<(NodeId, Body)>,

    // Recorder and log.
    registers: BTreeMap<Slot, Register>,
    log: BTreeMap<Slot, Decision>,
    committed: u64,
    emitted: u64,
    peer_committed: u64,
    seen: BTreeMap<Slot, Timestamp>,
    /// When the outstanding catch-up request was sent.
    catch_up_sent: Option<Timestamp>,

    // Proposer side.
    proposers: BTreeMap<Slot, Active>,
    batches: BTreeMap<u64, LocalBatch>,
    queue: VecDeque<Entry>,
    outstanding: BTreeSet<ProposalId>,
    next_batch: u64,
    next_proposal: u64,
    next_alloc: u64,
    /// First slot this incarnation may use the leader priority in.
    h_floor: u64,
    reserved_through: Option<u64>,
    reserved_durable: Option<u64>,
}

impl Replica {
    /// Starts a replica from the durable records of its previous incarnations (in the
    /// order they were persisted; empty for a fresh replica).
    pub fn new(
        cfg: Config,
        durable: impl IntoIterator<Item = Bytes>,
        now: Timestamp,
    ) -> Result<Self, Error> {
        cfg.validate()?;
        let mut last_incarnation = 0;
        let mut registers = BTreeMap::new();
        let mut log = BTreeMap::new();
        let mut reservation: Option<u64> = None;
        for (index, bytes) in durable.into_iter().enumerate() {
            match Record::decode(&bytes).map_err(|_| Error::CorruptRecord { index })? {
                Record::Incarnation(n) => last_incarnation = last_incarnation.max(n),
                Record::Recorder { slot, reg } => {
                    registers.insert(slot, reg);
                }
                Record::Reservation { through } => reservation = reservation.max(Some(through.0)),
                Record::Decided { slot, proposal } => {
                    log.insert(
                        slot,
                        Decision {
                            proposal,
                            durable: true,
                        },
                    );
                }
            }
        }
        registers.retain(|slot, _| !log.contains_key(slot));
        let me = cfg.id;
        let leader = cfg.members.iter().copied().min().unwrap_or(me);
        let wire = WireCtx {
            cluster_id: cfg.cluster_id,
            config_epoch: 0,
            max_message_bytes: cfg.max_message_bytes,
        };
        let rng = ChaCha20::new(cfg.seed);
        let mut r = Self {
            cfg,
            me,
            leader,
            wire,
            rng,
            incarnation: last_incarnation + 1,
            now,
            fenced: false,
            metrics: Metrics::default(),
            ready: false,
            next_persist: 0,
            acked: None,
            writes: Vec::new(),
            gated: Vec::new(),
            effects: Vec::new(),
            in_flight: VecDeque::new(),
            actions: Vec::new(),
            inbox: VecDeque::new(),
            registers,
            log,
            committed: 0,
            emitted: 0,
            peer_committed: 0,
            seen: BTreeMap::new(),
            catch_up_sent: None,
            proposers: BTreeMap::new(),
            batches: BTreeMap::new(),
            queue: VecDeque::new(),
            outstanding: BTreeSet::new(),
            next_batch: 0,
            next_proposal: 0,
            next_alloc: 0,
            h_floor: reservation.map_or(0, |r| r + 1),
            reserved_through: None,
            reserved_durable: None,
        };
        r.advance_committed();
        // Slots with recovered recorder state were started by someone; make sure they get driven.
        let started: Vec<Slot> = r.registers.keys().copied().collect();
        for slot in started {
            r.seen.entry(slot).or_insert(now);
        }
        r.emit_commits();
        // Nothing leaves this replica until its new incarnation is durable.
        r.writes.push(Record::Incarnation(r.incarnation).encode());
        r.effects.push(Effect::Ready);
        r.flush();
        Ok(r)
    }

    /// This replica's id.
    pub fn id(&self) -> NodeId {
        self.me
    }

    /// Number of contiguous decided slots from the start of the log.
    pub fn commit_index(&self) -> u64 {
        self.committed
    }

    /// Counters for observability.
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// Queues `payload` for the log. The returned id later appears in exactly one of
    /// [`Action::Committed`] or [`Action::Dropped`], unless the replica crashes first.
    pub fn propose(&mut self, now: Timestamp, payload: Bytes) -> Result<ProposalId, Error> {
        self.begin(now)?;
        if self.fenced {
            return Err(Error::Fenced);
        }
        if self.outstanding.len() >= self.cfg.max_outstanding_proposals as usize {
            return Err(Error::Busy);
        }
        let id = ProposalId(self.next_proposal);
        let entry = Entry {
            origin: self.me,
            incarnation: self.incarnation,
            id,
            payload,
        };
        if 4 + batch::encoded_len(&entry) > self.cfg.max_batch_bytes as usize {
            return Err(Error::PayloadTooLarge);
        }
        self.next_proposal += 1;
        self.outstanding.insert(id);
        self.queue.push_back(entry);
        self.pump();
        self.flush();
        Ok(id)
    }

    /// Feeds one external event.
    pub fn handle(&mut self, now: Timestamp, input: Input) -> Result<(), Error> {
        self.begin(now)?;
        if self.fenced {
            return Ok(());
        }
        match input {
            Input::Message(bytes) => self.on_message(&bytes),
            Input::Persisted(id) => self.on_persisted(id)?,
            Input::StorageFailed(_) => self.fence(),
        }
        self.pump();
        self.flush();
        Ok(())
    }

    /// Fires timers due at `now`; call it at or after [`Replica::next_deadline`].
    pub fn handle_timeout(&mut self, now: Timestamp) -> Result<(), Error> {
        self.begin(now)?;
        if self.fenced {
            return Ok(());
        }
        let due: Vec<Slot> = self
            .proposers
            .iter()
            .filter(|(_, a)| a.resend_at <= now)
            .map(|(s, _)| *s)
            .collect();
        for slot in due {
            self.metrics.retransmits += 1;
            self.send_requests(slot, true);
        }
        self.drive_stalled();
        self.pump();
        self.flush();
        Ok(())
    }

    /// When [`Replica::handle_timeout`] next needs to run, if at all.
    pub fn next_deadline(&self) -> Option<Timestamp> {
        if self.fenced {
            return None;
        }
        let resend = self.proposers.values().map(|a| a.resend_at);
        let stall = self
            .stalled_candidates()
            .map(|(_, at)| at.after(self.cfg.stall_timeout));
        resend.chain(stall).min()
    }

    /// Moves every pending action into `out`. Must be called after each other method
    /// before the next one.
    pub fn poll_actions(&mut self, out: &mut Vec<Action>) {
        out.append(&mut self.actions);
    }

    fn begin(&mut self, now: Timestamp) -> Result<(), Error> {
        if !self.actions.is_empty() {
            return Err(Error::NotDrained);
        }
        self.now = self.now.max(now);
        Ok(())
    }

    fn window(&self) -> u64 {
        u64::from(self.cfg.max_in_flight_slots)
    }

    // ---- durability gate ----

    fn idle_storage(&self) -> bool {
        self.ready && self.writes.is_empty() && self.in_flight.is_empty()
    }

    /// Runs `e` now if nothing is waiting on storage, else once the next `Persist` lands.
    fn after_durable(&mut self, e: Effect) {
        if self.idle_storage() {
            self.apply(e);
        } else {
            self.effects.push(e);
        }
    }

    /// Emits a `Persist` for new writes. Sends and effects that only wait for writes
    /// already in flight are parked behind the last one instead, so idle replies never
    /// cost an extra (empty) write.
    fn flush(&mut self) {
        if self.writes.is_empty() {
            if let Some(last) = self.in_flight.back_mut() {
                last.effects.append(&mut self.effects);
                last.parked.append(&mut self.gated);
                return;
            }
            debug_assert!(
                self.gated.is_empty() && self.effects.is_empty(),
                "gated work without a write to wait for"
            );
            return;
        }
        let id = self.next_persist;
        self.next_persist += 1;
        self.actions.push(Action::Persist {
            id: PersistId(id),
            records: mem::take(&mut self.writes),
            then: mem::take(&mut self.gated),
        });
        self.in_flight.push_back(InFlight {
            id,
            effects: mem::take(&mut self.effects),
            parked: Vec::new(),
        });
    }

    fn on_persisted(&mut self, PersistId(id): PersistId) -> Result<(), Error> {
        if id >= self.next_persist || self.acked.is_some_and(|a| id <= a) {
            return Err(Error::UnknownPersistId);
        }
        self.acked = Some(id);
        while self.in_flight.front().is_some_and(|f| f.id <= id) {
            let Some(done) = self.in_flight.pop_front() else {
                break;
            };
            self.actions
                .extend(done.parked.into_iter().map(Action::Send));
            for e in done.effects {
                self.apply(e);
            }
        }
        Ok(())
    }

    fn apply(&mut self, e: Effect) {
        match e {
            Effect::Ready => self.ready = true,
            Effect::ReservationDurable(t) => {
                self.reserved_durable = self.reserved_durable.max(Some(t))
            }
            Effect::DecidedDurable(slot) => {
                if let Some(d) = self.log.get_mut(&slot) {
                    d.durable = true;
                }
                self.registers.remove(&slot);
                self.emit_commits();
            }
            Effect::LocalReply(body) => self.inbox.push_back((self.me, body)),
        }
    }

    fn fence(&mut self) {
        self.fenced = true;
        for id in mem::take(&mut self.outstanding) {
            self.actions.push(Action::Dropped {
                id,
                reason: DropReason::Fenced,
            });
        }
        self.proposers.clear();
        self.queue.clear();
        self.batches.clear();
        self.inbox.clear();
        self.writes.clear();
        self.gated.clear();
        self.effects.clear();
    }

    // ---- network ----

    fn encode(&self, to: NodeId, body: Body) -> Outbound {
        let env = Envelope {
            cluster_id: self.cfg.cluster_id,
            config_epoch: 0,
            from: self.me,
            to,
            commit_index: self.committed,
        };
        Outbound {
            to,
            bytes: Message { env, body }.encode(),
        }
    }

    /// A send that does not depend on durable state.
    fn send(&mut self, to: NodeId, body: Body) {
        if to == self.me {
            self.inbox.push_back((to, body));
        } else if self.ready {
            let out = self.encode(to, body);
            self.actions.push(Action::Send(out));
        } else {
            let out = self.encode(to, body);
            self.gated.push(out);
        }
    }

    /// A send that reports state possibly not yet durable.
    fn send_gated(&mut self, to: NodeId, body: Body) {
        let out = self.encode(to, body);
        if self.idle_storage() {
            self.actions.push(Action::Send(out));
        } else {
            self.gated.push(out);
        }
    }

    fn on_message(&mut self, bytes: &Bytes) {
        let Ok(msg) = Message::decode(bytes, &self.wire) else {
            self.metrics.messages_rejected += 1;
            return;
        };
        let from = msg.env.from;
        if msg.env.to != self.me || from == self.me || !self.cfg.members.contains(&from) {
            self.metrics.messages_rejected += 1;
            return;
        }
        if msg.env.commit_index > self.peer_committed {
            self.peer_committed = msg.env.commit_index;
        }
        if self.peer_committed > self.committed {
            self.seen.entry(Slot(self.committed)).or_insert(self.now);
        }
        // More than the slot in progress behind: fetch the decided prefix in bulk.
        let idle = self
            .catch_up_sent
            .is_none_or(|t| t.after(self.cfg.retransmit_interval) <= self.now);
        if msg.env.commit_index > self.committed + 1 && idle {
            self.catch_up_sent = Some(self.now);
            self.send(
                from,
                Body::CatchUp {
                    from: Slot(self.committed),
                },
            );
        }
        self.inbox.push_back((from, msg.body));
    }

    fn pump(&mut self) {
        loop {
            while let Some((from, body)) = self.inbox.pop_front() {
                match body {
                    Body::RecordRequest {
                        slot,
                        step,
                        proposal,
                    } => self.on_request(from, slot, step, proposal),
                    Body::RecordReply {
                        slot,
                        req_step,
                        step,
                        first,
                        prev_agg,
                    } => self.on_reply(from, slot, req_step, step, first, prev_agg),
                    Body::Decided { slot, proposal } => self.learn(slot, proposal, false),
                    Body::CatchUp { from: start } => self.on_catch_up(from, start),
                    Body::Entries { first, proposals } => {
                        self.catch_up_sent = None;
                        for (slot, p) in (first.0..).zip(proposals) {
                            self.learn(Slot(slot), p, false);
                        }
                    }
                }
            }
            self.schedule();
            if self.inbox.is_empty() {
                break;
            }
        }
    }

    /// Serves contiguous decided slots from `start`, within half a message budget.
    fn on_catch_up(&mut self, to: NodeId, start: Slot) {
        let budget = self.cfg.max_message_bytes as usize / 2;
        let mut size = 0;
        let mut proposals = Vec::new();
        for slot in start.0.. {
            let Some(d) = self.log.get(&Slot(slot)) else {
                break;
            };
            size += d.proposal.value.len() + 64;
            if !proposals.is_empty() && size > budget {
                break;
            }
            proposals.push(d.proposal.clone());
        }
        if !proposals.is_empty() {
            self.send(
                to,
                Body::Entries {
                    first: start,
                    proposals,
                },
            );
        }
    }

    // ---- recorder ----

    fn on_request(&mut self, from: NodeId, slot: Slot, step: u32, proposal: Proposal) {
        if let Some(d) = self.log.get(&slot) {
            let proposal = d.proposal.clone();
            self.send(from, Body::Decided { slot, proposal });
            return;
        }
        if slot.0 < self.committed || slot.0 >= self.committed + 4 * self.window() {
            return;
        }
        self.seen.entry(slot).or_insert(self.now);
        let key = proposal.key;
        let reg = self.registers.entry(slot).or_default();
        if reg.record(step, proposal) {
            let rec = Record::Recorder {
                slot,
                reg: reg.clone(),
            }
            .encode();
            self.writes.push(rec);
        }
        let s = reg.summary();
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

    // ---- proposer ----

    fn on_reply(
        &mut self,
        from: NodeId,
        slot: Slot,
        req_step: u32,
        step: u32,
        first: Option<WireProposal>,
        prev_agg: Option<WireProposal>,
    ) {
        let Some(active) = self.proposers.get_mut(&slot) else {
            return;
        };
        if req_step != active.prop.step() {
            return;
        }
        let Some(request) = active.prop.request_for(from).cloned() else {
            return;
        };
        let resolve =
            |w: Option<WireProposal>| w.map(|w| w.resolve(&request).ok_or(())).transpose();
        let (Ok(first), Ok(prev_agg)) = (resolve(first), resolve(prev_agg)) else {
            self.metrics.messages_rejected += 1;
            return;
        };
        match active.prop.on_reply(
            from,
            Reply {
                req_step,
                step,
                first,
                prev_agg,
            },
            &mut self.rng,
        ) {
            Progress::Waiting => {}
            Progress::Advanced => self.send_requests(slot, false),
            Progress::Decided(p) => {
                if req_step == 4 && p.priority == LEADER_PRIORITY {
                    self.metrics.fast_path_decisions += 1;
                }
                self.learn(slot, p, true);
            }
        }
    }

    /// Sends the current step's unanswered requests; `resend` skips the local recorder,
    /// whose delivery is reliable.
    fn send_requests(&mut self, slot: Slot, resend: bool) {
        let me = self.me;
        let resend_at = self.now.after(self.cfg.retransmit_interval);
        let h_ready = self.reserved_durable.is_some_and(|t| slot.0 <= t);
        let Some(a) = self.proposers.get_mut(&slot) else {
            return;
        };
        a.resend_at = resend_at;
        let step = a.prop.step();
        let reqs: Vec<(NodeId, Proposal)> = a
            .prop
            .requests()
            .filter(|(j, _)| !(resend && *j == me))
            .map(|(j, p)| (j, p.clone()))
            .collect();
        for (j, proposal) in reqs {
            // The leader's own top-priority proposal (only ever sent at the first step) may
            // leave only once its reservation is durable; later phases may forward it freely.
            let gate = step == 4 && proposal.priority == LEADER_PRIORITY && !h_ready && j != me;
            let body = Body::RecordRequest {
                slot,
                step,
                proposal,
            };
            if gate {
                let out = self.encode(j, body);
                self.gated.push(out);
            } else {
                self.send(j, body);
            }
        }
    }

    fn learn(&mut self, slot: Slot, proposal: Proposal, broadcast: bool) {
        if slot.0 < self.emitted || self.log.contains_key(&slot) {
            return;
        }
        self.metrics.slots_decided += 1;
        self.writes.push(
            Record::Decided {
                slot,
                proposal: proposal.clone(),
            }
            .encode(),
        );
        // A local batch that lost its slot goes back to be proposed elsewhere.
        if let Some(a) = self.proposers.remove(&slot)
            && let Some(b) = self.batches.get_mut(&a.batch)
            && b.key != proposal.key
        {
            b.slot = None;
            if b.ids.is_empty() {
                self.batches.remove(&a.batch);
            }
        }
        for gap in self.committed..slot.0 {
            self.seen.entry(Slot(gap)).or_insert(self.now);
        }
        if broadcast {
            let peers: Vec<NodeId> = self
                .cfg
                .members
                .iter()
                .copied()
                .filter(|&m| m != self.me)
                .collect();
            for peer in peers {
                self.send(
                    peer,
                    Body::Decided {
                        slot,
                        proposal: proposal.clone(),
                    },
                );
            }
        }
        self.log.insert(
            slot,
            Decision {
                proposal,
                durable: false,
            },
        );
        self.advance_committed();
        self.after_durable(Effect::DecidedDurable(slot));
    }

    fn advance_committed(&mut self) {
        while self.log.contains_key(&Slot(self.committed)) {
            self.committed += 1;
        }
        let floor = Slot(self.committed);
        self.seen = self.seen.split_off(&floor);
    }

    fn emit_commits(&mut self) {
        while let Some(d) = self.log.get(&Slot(self.emitted)) {
            if !d.durable {
                break;
            }
            let slot = Slot(self.emitted);
            let key = d.proposal.key;
            let entries = batch::decode(&d.proposal.value).unwrap_or_else(|_| {
                self.metrics.corrupt_batches += 1;
                Vec::new()
            });
            let mine: Vec<ProposalId> = entries
                .iter()
                .filter(|e| e.origin == self.me && e.incarnation == self.incarnation)
                .map(|e| e.id)
                .filter(|id| self.outstanding.remove(id))
                .collect();
            self.actions.push(Action::Commit { slot, entries });
            for id in mine {
                self.actions.push(Action::Committed { id, slot });
            }
            if key.origin == self.me && key.incarnation == self.incarnation {
                self.batches.remove(&key.seq);
            }
            self.emitted += 1;
        }
    }

    // ---- scheduling ----

    fn schedule(&mut self) {
        if self.fenced {
            return;
        }
        while (self.proposers.len() as u64) < self.window() {
            let seq = match self
                .batches
                .iter()
                .find(|(_, b)| b.slot.is_none() && !b.ids.is_empty())
            {
                Some((seq, _)) => *seq,
                None if !self.queue.is_empty() => self.cut_batch(),
                None => break,
            };
            let Some(slot) = self.alloc_slot() else { break };
            self.start(slot, seq);
        }
    }

    fn cut_batch(&mut self) -> u64 {
        let mut entries = Vec::new();
        let mut size = 4;
        while let Some(e) = self.queue.front() {
            let len = batch::encoded_len(e);
            if !entries.is_empty() && size + len > self.cfg.max_batch_bytes as usize {
                break;
            }
            size += len;
            entries.extend(self.queue.pop_front());
        }
        self.new_batch(entries)
    }

    fn new_batch(&mut self, entries: Vec<Entry>) -> u64 {
        let seq = self.next_batch;
        self.next_batch += 1;
        let key = ValueKey {
            origin: self.me,
            incarnation: self.incarnation,
            seq,
        };
        let ids = entries.iter().map(|e| e.id).collect();
        self.batches.insert(
            seq,
            LocalBatch {
                key,
                value: batch::encode(&entries),
                ids,
                slot: None,
            },
        );
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
        let Some(b) = self.batches.get_mut(&seq) else {
            return;
        };
        b.slot = Some(slot);
        let proposal = Proposal {
            priority: 0,
            key: b.key,
            value: b.value.clone(),
        };
        let leader = self.leader == self.me && slot.0 >= self.h_floor;
        if leader && self.reserved_through.is_none_or(|t| slot.0 > t) {
            let through = slot.0 + 4 * self.window();
            self.reserved_through = Some(through);
            self.writes.push(
                Record::Reservation {
                    through: Slot(through),
                }
                .encode(),
            );
            self.after_durable(Effect::ReservationDurable(through));
        }
        let prop = SlotProposer::new(self.cfg.members.clone(), leader, proposal, &mut self.rng);
        self.proposers.insert(
            slot,
            Active {
                prop,
                batch: seq,
                resend_at: self.now,
            },
        );
        self.send_requests(slot, false);
    }

    /// Undecided slots in the window that someone else started, with when we noticed them.
    fn stalled_candidates(&self) -> impl Iterator<Item = (Slot, Timestamp)> + '_ {
        let end = Slot(self.committed + self.window());
        self.seen
            .range(..end)
            .filter(|(s, _)| !self.log.contains_key(s) && !self.proposers.contains_key(s))
            .map(|(s, t)| (*s, *t))
    }

    /// Drives slots that stalled (their proposer may be gone) so the log never has a gap.
    fn drive_stalled(&mut self) {
        let deadline = self.cfg.stall_timeout;
        let due: Vec<Slot> = self
            .stalled_candidates()
            .filter(|(_, t)| t.after(deadline) <= self.now)
            .map(|(s, _)| s)
            .collect();
        for slot in due {
            let seq = self.new_batch(Vec::new());
            self.start(slot, seq);
        }
    }
}

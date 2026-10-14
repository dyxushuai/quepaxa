//! The sans-IO replica: one proposer and one recorder per slot, a decided log, and the
//! durability gate that keeps replies behind the writes they depend on (ADR 0002).
//!
//! `gate` holds the durability gate, `log` the recorder and decided-log side, `proposing` the proposer side (batching,
//! forwarding to the leader, hedging, stalled slots).

mod gate;
mod log;
mod proposing;
mod snapshot;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use bytes::Bytes;
use core::mem;

use self::gate::Gate;

use crate::api::{Action, Config, DropReason, Error, Input, Metrics, Outbound};
use crate::batch::{self, Entry, Item};
use crate::codec::{Body, Envelope, Message, WireCtx};
use crate::core_state::CoreState;
use crate::dedup::Dedup;
use crate::durable::Record;
use crate::proposal::{Proposal, ValueKey};
use crate::proposer::SlotProposer;
use crate::recorder::Register;
use crate::rng::ChaCha20;
use crate::schedule::Schedule;
use crate::types::{MAX_SLOT, MAX_STEP, NodeId, PersistId, ProposalId, Slot, Timestamp};

#[cfg_attr(feature = "model", derive(Clone, Hash))]
struct Decision {
    proposal: Proposal,
    durable: bool,
}

#[cfg_attr(feature = "model", derive(Clone, Hash))]
struct Active {
    prop: SlotProposer,
    batch: u64,
    resend_at: Timestamp,
}

/// A batch this replica proposes, in at most one slot at a time until it is decided.
#[cfg_attr(feature = "model", derive(Clone, Hash))]
struct LocalBatch {
    key: ValueKey,
    value: Bytes,
    entries: Vec<(NodeId, u64, ProposalId)>,
    slot: Option<Slot>,
}

/// Work deferred until the next `Persist` is acknowledged.
#[cfg_attr(feature = "model", derive(Clone, Hash))]
enum Effect {
    ReservationDurable(u64),
    DecidedDurable(Slot),
    LocalReply(Body),
}

/// One member of a QuePaxa cluster. See the crate docs for the driving contract.
#[cfg_attr(feature = "model", derive(Clone, Hash))]
pub struct Replica {
    cfg: Config,
    me: NodeId,
    wire: WireCtx,
    rng: ChaCha20,
    incarnation: u64,
    now: Timestamp,
    fenced: bool,
    metrics: Metrics,

    gate: Gate<Effect>,
    actions: Vec<Action>,
    inbox: VecDeque<(NodeId, Body)>,

    // Recorder and decided log.
    registers: BTreeMap<Slot, Register>,
    log: BTreeMap<Slot, Decision>,
    committed: u64,
    emitted: u64,
    peer_committed: u64,
    /// Undecided slots someone else drives, with when they last made progress.
    seen: BTreeMap<Slot, Timestamp>,
    /// Outstanding bulk catch-up request: peer and when it was sent.
    catch_up: Option<(NodeId, Timestamp)>,
    /// A peer that advertised `peer_committed`.
    ahead_peer: Option<NodeId>,
    /// Leader schedule as of the decided prefix (`committed`), used to drive slots.
    schedule: Schedule,
    /// The same schedule as of the emitted prefix (`emitted`), carried in `core_state`.
    applied_schedule: Schedule,
    dedup: Dedup,

    // Proposer side.
    proposers: BTreeMap<Slot, Active>,
    batches: BTreeMap<u64, LocalBatch>,
    /// Entries to batch if this replica leads, else to forward to the leader.
    pending: VecDeque<Item>,
    /// Own entries the leader did not commit in time; this replica proposes them itself.
    hedged: VecDeque<Item>,
    /// Own entries handed to the leader, with when.
    forwarded: BTreeMap<ProposalId, (Item, Timestamp)>,
    /// Own proposals awaiting commit: when they were made, and whether it is a barrier.
    outstanding: BTreeMap<ProposalId, (Timestamp, bool)>,
    /// Smoothed propose-to-commit latency of own proposals, in nanoseconds.
    commit_latency: Option<u64>,
    next_batch: u64,
    next_proposal: u64,
    next_alloc: u64,
    /// First slot this incarnation may use the leader priority in.
    h_floor: u64,
    reserved_through: Option<u64>,
    reserved_durable: Option<u64>,

    // Compaction.
    /// First slot not covered by a snapshot; nothing below it is kept.
    floor: u64,
    /// Core state of the latest snapshot, carried into checkpoints.
    snapshot_state: Option<Bytes>,
    snapshot_asked: Option<Timestamp>,
    /// Steps at which this replica's proposers decided, for verification only.
    #[cfg(feature = "model")]
    decided_steps: BTreeMap<Slot, (Proposal, u32)>,
}

impl Replica {
    /// Starts a replica from the durable records of its previous incarnations (in the
    /// order they were persisted; empty for a fresh replica).
    pub fn new(cfg: Config, durable: impl IntoIterator<Item = Bytes>, now: Timestamp) -> Result<Self, Error> {
        cfg.validate()?;
        let mut last_incarnation = 0;
        let mut registers = BTreeMap::new();
        let mut log = BTreeMap::new();
        let mut reservation: Option<u64> = None;
        let mut snapshot: Option<(CoreState, Bytes)> = None;
        for (index, bytes) in durable.into_iter().enumerate() {
            match Record::decode(&bytes).map_err(|_| Error::CorruptRecord { index })? {
                Record::Incarnation(n) => last_incarnation = last_incarnation.max(n),
                Record::Recorder { slot, reg } => {
                    if reg.step > MAX_STEP || slot.0 > MAX_SLOT {
                        return Err(Error::CorruptRecord { index });
                    }
                    registers.insert(slot, reg);
                }
                Record::Reservation { through } => reservation = reservation.max(Some(through.0)),
                Record::Decided { slot, proposal } => {
                    log.insert(slot, Decision { proposal, durable: true });
                }
                Record::Compacted { through, core_state } => {
                    let state = snapshot::decode_state(through, &core_state, &cfg.members)
                        .ok_or(Error::CorruptRecord { index })?;
                    if snapshot.as_ref().is_none_or(|(s, _)| s.next < state.next) {
                        snapshot = Some((state, core_state));
                    }
                }
            }
        }
        let floor = snapshot.as_ref().map_or(0, |(s, _)| s.next);
        log = log.split_off(&Slot(floor));
        registers = registers.split_off(&Slot(floor));
        registers.retain(|slot, _| !log.contains_key(slot));
        let wire = WireCtx {
            cluster_id: cfg.wire_cluster_id(),
            config_epoch: 0,
            max_message_bytes: cfg.max_message_bytes,
            max_value_bytes: cfg.max_batch_bytes,
        };
        let mut r = Self {
            me: cfg.id,
            wire,
            rng: ChaCha20::new(cfg.seed),
            schedule: snapshot
                .as_ref()
                .map_or_else(|| Schedule::with_order(cfg.leaders()), |(s, _)| s.schedule.clone()),
            applied_schedule: snapshot
                .as_ref()
                .map_or_else(|| Schedule::with_order(cfg.leaders()), |(s, _)| s.schedule.clone()),
            cfg,
            incarnation: last_incarnation + 1,
            now,
            fenced: false,
            metrics: Metrics::default(),
            gate: Gate::new(Record::Incarnation(last_incarnation + 1).encode()),
            actions: Vec::new(),
            inbox: VecDeque::new(),
            registers,
            log,
            committed: floor,
            emitted: floor,
            peer_committed: 0,
            seen: BTreeMap::new(),
            catch_up: None,
            ahead_peer: None,
            dedup: snapshot.as_ref().map_or_else(Dedup::default, |(s, _)| s.dedup.clone()),
            proposers: BTreeMap::new(),
            batches: BTreeMap::new(),
            pending: VecDeque::new(),
            hedged: VecDeque::new(),
            forwarded: BTreeMap::new(),
            outstanding: BTreeMap::new(),
            commit_latency: None,
            next_batch: 0,
            next_proposal: 0,
            next_alloc: 0,
            h_floor: reservation.map_or(0, |r| r + 1),
            reserved_through: None,
            reserved_durable: None,
            floor,
            snapshot_state: snapshot.map(|(_, b)| b),
            snapshot_asked: None,
            #[cfg(feature = "model")]
            decided_steps: BTreeMap::new(),
        };
        r.advance_committed();
        // Slots with recovered recorder state were started by someone; make sure they get driven.
        let started: Vec<Slot> = r.registers.keys().copied().collect();
        for slot in started {
            r.seen.entry(slot).or_insert(now);
        }
        r.emit_commits();
        // Nothing leaves this replica until its new incarnation is durable.
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
        self.enqueue(now, payload, false)
    }

    /// Requests a linearizable read point. The returned id later appears in exactly one of
    /// [`Action::ReadReady`] or [`Action::Dropped`]: once the application has applied every
    /// commit up to the reported slot, a read of its state is linearizable.
    pub fn read_barrier(&mut self, now: Timestamp) -> Result<ProposalId, Error> {
        self.enqueue(now, Bytes::new(), true)
    }

    fn enqueue(&mut self, now: Timestamp, payload: Bytes, barrier: bool) -> Result<ProposalId, Error> {
        self.begin(now)?;
        if self.fenced {
            return Err(Error::Fenced);
        }
        if self.outstanding.len() >= self.cfg.max_outstanding_proposals as usize {
            return Err(Error::Busy);
        }
        let id = ProposalId(self.next_proposal);
        let item =
            Item { entry: Entry { origin: self.me, incarnation: self.incarnation, id, payload }, barrier };
        if !batch::fits_alone(&item, self.cfg.max_batch_bytes as usize) {
            return Err(Error::PayloadTooLarge);
        }
        self.next_proposal += 1;
        self.outstanding.insert(id, (self.now, barrier));
        self.pending.push_back(item);
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
        self.fire_timers();
        self.pump();
        self.flush();
        Ok(())
    }

    /// When [`Replica::handle_timeout`] next needs to run, if at all.
    pub fn next_deadline(&self) -> Option<Timestamp> {
        if self.fenced { None } else { self.earliest_timer() }
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

    /// Runs `e` now if nothing is waiting on storage, else once it is durable.
    fn after_durable(&mut self, e: Effect) {
        if let Some(e) = self.gate.after_durable(e) {
            self.apply(e);
        }
    }

    fn flush(&mut self) {
        self.metrics.retained_slots = self.log.len() as u64;
        self.metrics.live_registers = self.registers.len() as u64;
        self.gate.flush(&mut self.actions);
    }

    fn on_persisted(&mut self, id: PersistId) -> Result<(), Error> {
        for e in self.gate.persisted(id, &mut self.actions)? {
            self.apply(e);
        }
        Ok(())
    }

    fn apply(&mut self, e: Effect) {
        match e {
            Effect::ReservationDurable(t) => self.reserved_durable = self.reserved_durable.max(Some(t)),
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
        for id in mem::take(&mut self.outstanding).into_keys() {
            self.actions.push(Action::Dropped { id, reason: DropReason::Fenced });
        }
        self.proposers.clear();
        self.pending.clear();
        self.hedged.clear();
        self.forwarded.clear();
        self.batches.clear();
        self.inbox.clear();
        self.gate.fence();
    }

    // ---- network ----

    fn encode(&self, to: NodeId, body: Body) -> Outbound {
        let env = Envelope {
            cluster_id: self.wire.cluster_id,
            config_epoch: 0,
            from: self.me,
            to,
            commit_index: self.committed,
        };
        Outbound { to, bytes: Message { env, body }.encode() }
    }

    /// A send that does not depend on durable state.
    fn send(&mut self, to: NodeId, body: Body) {
        if to == self.me {
            self.inbox.push_back((to, body));
            return;
        }
        let out = self.encode(to, body);
        self.gate.send(out, &mut self.actions);
    }

    /// A send that reports state possibly not yet durable.
    fn send_gated(&mut self, to: NodeId, body: Body) {
        let out = self.encode(to, body);
        self.gate.send_after_durable(out, &mut self.actions);
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
            self.ahead_peer = Some(from);
        }
        if self.peer_committed > self.committed {
            self.seen.entry(Slot(self.committed)).or_insert(self.now);
        }
        if msg.env.commit_index > self.committed + 1 && self.catch_up_due() {
            self.request_catch_up(from);
        }
        self.inbox.push_back((from, msg.body));
    }

    /// Whether a bulk catch-up request may be (re)sent now.
    fn catch_up_due(&self) -> bool {
        self.catch_up.is_none_or(|(_, t)| t.after(self.cfg.retransmit_interval) <= self.now)
    }

    /// Fetches the decided prefix from `committed` in bulk.
    fn request_catch_up(&mut self, peer: NodeId) {
        self.catch_up = Some((peer, self.now));
        self.send(peer, Body::CatchUp { from: Slot(self.committed) });
    }

    fn pump(&mut self) {
        loop {
            while let Some((from, body)) = self.inbox.pop_front() {
                match body {
                    Body::RecordRequest { slot, step, proposal } => {
                        self.on_request(from, slot, step, proposal)
                    }
                    Body::RecordReply { slot, req_step, step, first, prev_agg } => {
                        self.on_reply(from, slot, req_step, step, first, prev_agg)
                    }
                    Body::Decided { slot, proposal } => self.learn(slot, proposal, false),
                    Body::CatchUp { from: start } => self.on_catch_up(from, start),
                    Body::Entries { first, proposals } => {
                        for (i, p) in proposals.into_iter().enumerate() {
                            let Some(slot) = first.0.checked_add(i as u64) else { break };
                            self.learn(Slot(slot), p, false);
                        }
                        // A late or duplicate page from another peer is still useful, but only
                        // the active request's answer moves the catch-up along.
                        if self.catch_up.is_some_and(|(peer, _)| peer == from) {
                            self.catch_up = None;
                            // Still behind after one page: ask the same peer for the next one.
                            if self.peer_committed > self.committed + 1 {
                                self.request_catch_up(from);
                            }
                        }
                    }
                    Body::Forward { batch } => self.on_forward(&batch),
                    Body::Compacted { through } => self.on_compacted(from, through),
                }
            }
            self.drive();
            if self.inbox.is_empty() {
                break;
            }
        }
    }
}

#[cfg(feature = "model")]
impl Replica {
    /// Protocol-internal state for invariant checking in tests only.
    #[doc(hidden)]
    pub fn inspect(&self) -> crate::inspect::Inspection {
        crate::inspect::Inspection {
            registers: self.registers.iter().map(|(s, r)| (s.0, r.into())).collect(),
            decided_here: self.decided_steps.iter().map(|(s, (p, step))| (s.0, p.into(), *step)).collect(),
            decided: self.log.iter().map(|(s, d)| (s.0, (&d.proposal).into())).collect(),
            local_batches: self.batches.len(),
        }
    }

    /// Hash of the complete replica state, for state-space exploration in tests only.
    #[doc(hidden)]
    pub fn fingerprint(&self) -> u64 {
        struct Fnv(u64);
        impl core::hash::Hasher for Fnv {
            fn finish(&self) -> u64 {
                self.0
            }
            fn write(&mut self, bytes: &[u8]) {
                for &b in bytes {
                    self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
                }
            }
        }
        let mut h = Fnv(0xcbf2_9ce4_8422_2325);
        core::hash::Hash::hash(self, &mut h);
        core::hash::Hasher::finish(&h)
    }
}

#[cfg(test)]
mod tests;

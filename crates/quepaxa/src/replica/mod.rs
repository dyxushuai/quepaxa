//! The sans-IO replica: one proposer and one recorder per slot, a decided log, and the
//! durability gate that keeps replies behind the writes they depend on (ADR 0002).
//!
//! `gate` holds the durability gate, `log` the recorder and decided-log side, `proposing` the proposer side (batching,
//! forwarding to the leader, hedging, stalled slots).

mod gate;
mod log;
mod proposing;
mod retire;
mod snapshot;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use bytes::Bytes;
use core::mem;

use self::gate::Gate;

use crate::api::{Action, Config, DropReason, Error, Input, Metrics, Outbound};
use crate::batch::{self, Entry, Item, Kind};
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

/// One replica of a QuePaxa cluster: a recorder and a proposer for every slot of the log,
/// with the decided log, leader schedule and durability bookkeeping around them.
///
/// A `Replica` is a state machine without I/O. Each call takes the current time and may
/// queue [`Action`]s; take them with [`Replica::poll_actions`] before the next call and
/// carry them out in order. [`guide::driving`](crate::guide::driving) is the full contract;
/// the [crate docs](crate) have a runnable example.
///
/// `Replica` is `Send + Sync` and is driven from one place at a time.
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
    /// Own proposals awaiting commit: when they were made, and what they are.
    outstanding: BTreeMap<ProposalId, (Timestamp, Kind)>,
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

    // Retirement, while this replica is removed from the latest epoch.
    /// Durable progress each member of that epoch reported in catch-up replies.
    member_progress: BTreeMap<NodeId, u64>,
    /// When to ask the members still behind again.
    retire_probe: Option<Timestamp>,
    /// The epoch `Action::Retired` was reported for.
    retired: Option<u64>,
    /// Steps at which this replica's proposers decided, for verification only.
    #[cfg(feature = "model")]
    decided_steps: BTreeMap<Slot, (Proposal, u32)>,
}

impl Replica {
    /// Starts a replica from the durable records of its previous incarnations, in the
    /// order they were persisted (empty for a fresh replica).
    ///
    /// `cfg.seed` must be fresh CSPRNG output on every start. The startup actions are the
    /// [`Action::Commit`]s of the slots recovered from `durable` (emitted again, in order)
    /// followed by an [`Action::Persist`] of the new incarnation; nothing is sent before it
    /// is durable. See [`guide::durability`](crate::guide::durability).
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidConfig`] if `cfg` breaks a rule documented on [`Config`]; the
    ///   message says which.
    /// - [`Error::CorruptRecord`] if a record fails its checksum or does not decode;
    ///   `index` is its position in `durable`. Records are never skipped.
    /// - [`Error::InvalidConfig`] also if the durable state was built under another
    ///   [`Config::leader_order`].
    ///
    /// # Examples
    ///
    /// ```
    /// use quepaxa::{Action, Config, NodeId, Replica, Timestamp};
    ///
    /// let members = vec![NodeId(0), NodeId(1), NodeId(2)];
    /// let cfg = Config::new([1; 16], NodeId(0), members, [7; 32]);
    /// let mut replica = Replica::new(cfg, [], Timestamp(0))?;
    /// let mut actions = Vec::new();
    /// replica.poll_actions(&mut actions);
    /// assert!(matches!(actions[..], [Action::Persist { .. }]));
    /// # Ok::<(), quepaxa::Error>(())
    /// ```
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
                    log.insert(slot, Decision { proposal });
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
        // A checkpoint built under another leader order would disagree on who leads.
        if snapshot.as_ref().is_some_and(|(s, _)| s.leaders != cfg.leaders()) {
            return Err(Error::InvalidConfig(
                "leader_order differs from the one this replica's durable state was built with",
            ));
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
            member_progress: BTreeMap::new(),
            retire_probe: None,
            retired: None,
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

    /// Number of contiguous decided slots from the start of the log (including slots
    /// covered by a snapshot): the next slot to be decided in order.
    pub fn commit_index(&self) -> u64 {
        self.committed
    }

    /// Counters for observability; see [`Metrics`] and
    /// [`guide::tuning`](crate::guide::tuning) for what they indicate.
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// Queues `payload` to be appended to the log.
    ///
    /// The returned id later appears in exactly one of [`Action::Committed`] (the entry is
    /// in the log) or [`Action::Dropped`] — unless the replica crashes first, in which case
    /// the outcome is unknown. Entries are batched: concurrent proposals share slots. A
    /// replica that is not the leader forwards the entry to the leader, and proposes it
    /// itself if the leader does not commit it in time; it is committed at most once.
    ///
    /// # Errors
    ///
    /// - [`Error::Busy`] when [`Config::max_outstanding_proposals`] entries are already
    ///   waiting: back off and retry after some commit.
    /// - [`Error::PayloadTooLarge`] if the entry alone exceeds [`Config::max_batch_bytes`].
    /// - [`Error::Fenced`] after a storage failure; [`Error::NotDrained`] if the previous
    ///   call's actions were not taken.
    pub fn propose(&mut self, now: Timestamp, payload: Bytes) -> Result<ProposalId, Error> {
        self.enqueue(now, payload, Kind::App)
    }

    /// Requests a linearizable read point.
    ///
    /// The returned id later appears in exactly one of [`Action::ReadReady`] or
    /// [`Action::Dropped`]: once the application has applied every commit up to the
    /// reported slot, reading its state reflects every write that completed before this
    /// call, on any replica. See [`guide::reads`](crate::guide::reads).
    ///
    /// # Errors
    ///
    /// As [`Replica::propose`] ([`Error::Busy`], [`Error::Fenced`], [`Error::NotDrained`]);
    /// a barrier always fits a batch, so never [`Error::PayloadTooLarge`].
    pub fn read_barrier(&mut self, now: Timestamp) -> Result<ProposalId, Error> {
        self.enqueue(now, Bytes::new(), Kind::Barrier)
    }

    /// Proposes new membership: `members`, with `leader_order` as the preferred leaders
    /// (empty: by id).
    ///
    /// Once decided in slot `s` it takes effect at slot `s + 64`, reported by
    /// [`Action::Reconfigured`] in log order; the id is reported like a proposal's. A
    /// replica joining the cluster runs with the original [`Config`] (its own id need not
    /// be a member) and catches up before its first slot. See
    /// [`guide::membership`](crate::guide::membership).
    ///
    /// # Errors
    ///
    /// - [`Error::ReconfigurationPending`] while a change (decided, or proposed here) has
    ///   not taken effect. A concurrent change proposed elsewhere and decided after another
    ///   is ignored by every replica.
    /// - [`Error::InvalidConfig`] if `members` is empty, has duplicates or more than 1024
    ///   ids, or `leader_order` is not a permutation of `members`.
    /// - As [`Replica::propose`] otherwise.
    pub fn propose_reconfiguration(
        &mut self,
        now: Timestamp,
        members: Vec<NodeId>,
        leader_order: Vec<NodeId>,
    ) -> Result<ProposalId, Error> {
        let mut by_id = members.clone();
        by_id.sort_unstable();
        by_id.dedup();
        if members.is_empty() || by_id.len() != members.len() || members.len() > crate::schedule::MAX_MEMBERS
        {
            return Err(Error::InvalidConfig("members must be non-empty and unique"));
        }
        let order = if leader_order.is_empty() { by_id.clone() } else { leader_order };
        let mut sorted = order.clone();
        sorted.sort_unstable();
        if sorted != by_id {
            return Err(Error::InvalidConfig("leader_order must list every member exactly once"));
        }
        let proposed_here = self.outstanding.values().any(|(_, kind)| *kind == Kind::Reconfigure);
        if self.schedule.epoch().1 > self.committed || proposed_here {
            return Err(Error::ReconfigurationPending);
        }
        self.enqueue(now, crate::schedule::encode_order(&order), Kind::Reconfigure)
    }

    fn enqueue(&mut self, now: Timestamp, payload: Bytes, kind: Kind) -> Result<ProposalId, Error> {
        self.begin(now)?;
        if self.fenced {
            return Err(Error::Fenced);
        }
        if self.outstanding.len() >= self.cfg.max_outstanding_proposals as usize {
            return Err(Error::Busy);
        }
        let id = ProposalId(self.next_proposal);
        let item =
            Item { entry: Entry { origin: self.me, incarnation: self.incarnation, id, payload }, kind };
        if !batch::fits_alone(&item, self.cfg.max_batch_bytes as usize) {
            return Err(Error::PayloadTooLarge);
        }
        self.next_proposal += 1;
        self.outstanding.insert(id, (self.now, kind));
        self.pending.push_back(item);
        self.pump();
        self.flush();
        Ok(id)
    }

    /// Feeds one external event: a message from a peer, or the completion or failure of
    /// a [`Action::Persist`].
    ///
    /// Malformed messages and messages from other clusters or non-members are counted in
    /// [`Metrics::messages_rejected`] and otherwise ignored. After a storage failure the
    /// replica is fenced: it reports outstanding proposals as [`Action::Dropped`] and
    /// ignores further events.
    ///
    /// # Errors
    ///
    /// - [`Error::UnknownPersistId`] if `Persisted` names an id never issued or already
    ///   acknowledged.
    /// - [`Error::NotDrained`] if the previous call's actions were not taken.
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

    /// Fires the timers due at `now`: retransmission, hedging, slot takeover, catch-up.
    ///
    /// Call it when the clock reaches [`Replica::next_deadline`]. Calling it early is
    /// harmless (nothing that is not due happens); late only delays recovery.
    ///
    /// # Errors
    ///
    /// [`Error::NotDrained`] if the previous call's actions were not taken.
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

    /// When [`Replica::handle_timeout`] next needs to run; `None` if no timer is needed
    /// until the next call (and always after fencing). Re-read it after every call.
    pub fn next_deadline(&self) -> Option<Timestamp> {
        if self.fenced { None } else { self.earliest_timer() }
    }

    /// Moves every pending action into `out` (appending), in the order to carry them out.
    ///
    /// Call it after every other method and before the next one: a call made while actions
    /// are pending fails with [`Error::NotDrained`], so none is ever lost.
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
            // The recorder state is only garbage once the decision is durable here.
            Effect::DecidedDurable(slot) => {
                self.registers.remove(&slot);
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
        let known = self.cfg.members.contains(&from) || self.schedule.knows(from);
        if msg.env.to != self.me || from == self.me || !known {
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
        if let Body::Entries { .. } = msg.body {
            self.note_progress(from, msg.env.commit_index);
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
                        let page = !proposals.is_empty();
                        for (i, p) in proposals.into_iter().enumerate() {
                            let Some(slot) = first.0.checked_add(i as u64) else { break };
                            self.learn(Slot(slot), p, false);
                        }
                        // A late or duplicate page from another peer is still useful, but only
                        // the active request's answer moves the catch-up along.
                        if self.catch_up.is_some_and(|(peer, _)| peer == from) {
                            self.catch_up = None;
                            // Still behind after a page: ask the same peer for the next one
                            // (after an empty reply, only on the timer).
                            if page && self.peer_committed > self.committed + 1 {
                                self.request_catch_up(from);
                            }
                        }
                    }
                    Body::Forward { batch } => self.on_forward(&batch),
                    Body::Compacted { through } => self.on_compacted(from, through),
                }
            }
            self.drive();
            self.check_retired();
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

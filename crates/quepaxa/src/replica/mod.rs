//! The sans-IO replica: one proposer and one recorder per slot, a decided log, and the
//! durability gate that keeps replies behind the writes they depend on (ADR 0002).
//!
//! `gate` holds the durability gate, `log` the recorder and decided-log side, `messages`
//! sending and receiving, `recovery` the way from a start to normal operation (the phase,
//! catching up, the leader's reservation), `proposing` the proposer side (batching,
//! forwarding to the leader, hedging, stalled slots), `entries` where entries wait until
//! they commit.

mod disk;
mod entries;
mod gate;
mod log;
mod messages;
mod placement;
mod proposing;
mod ranking;
mod reads;
mod recovery;
mod retire;
mod snapshot;
mod watch;

use self::gate::Gate;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use bytes::Bytes;

use crate::api::{Action, Config, DropReason, Error, Input, MemberView, Metrics, Outbound};
use crate::batch::{self, Entry, Item, Kind};
use crate::codec::{Body, Envelope, Message, WireCtx};
use crate::content::{PinError, Store};
use crate::core_state::CoreState;
use crate::dedup::Dedup;
use crate::durable::Record;
use crate::proposal::Proposal;
use crate::proposer::SlotProposer;
use crate::recorder::Register;
use crate::rng::ChaCha20;
use crate::schedule::Schedule;
use crate::types::{MAX_SLOT, MAX_STEP, NodeId, PersistId, ProposalId, READ_ID_BASE, Slot, Timestamp};

#[cfg_attr(feature = "model", derive(Clone, Hash))]
struct Decision {
    proposal: Proposal,
}

#[cfg_attr(feature = "model", derive(Clone, Hash))]
struct Active {
    prop: SlotProposer,
    batch: u64,
    resend_at: Timestamp,
    /// When this replica started proposing in the slot.
    started: Timestamp,
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
    /// `Recovering → Live`, or `Fenced` (`recovery.rs`).
    phase: recovery::Phase,
    metrics: Metrics,

    gate: Gate<Effect>,
    actions: Vec<Action>,
    inbox: VecDeque<(NodeId, Body)>,

    // Recorder and decided log.
    registers: BTreeMap<Slot, Register>,
    log: BTreeMap<Slot, Decision>,
    committed: u64,
    emitted: u64,
    /// Undecided slots someone else drives, with when they last made progress.
    seen: BTreeMap<Slot, Timestamp>,
    /// Slots announced decided whose value this replica does not hold, with the peer last
    /// asked for it and when (`log.rs`).
    unresolved: BTreeMap<Slot, (NodeId, Timestamp)>,
    /// How far peers are ahead, and the bulk request in flight (`recovery.rs`).
    catch_up: recovery::CatchUp,
    /// Leader schedule as of the decided prefix (`committed`), used to drive slots.
    schedule: Schedule,
    /// The same schedule as of the emitted prefix (`emitted`), carried in `core_state`.
    applied_schedule: Schedule,
    dedup: Dedup,
    /// Content of the data batches that values reference (`content.rs`).
    content: Store,

    // Proposer side.
    proposers: BTreeMap<Slot, Active>,
    /// Own entries awaiting commit and those handed here, queued, forwarded or batched
    /// (`entries.rs`).
    entries: entries::Entries,
    /// Whether this replica suspects the leader it hands entries to (`watch.rs`).
    watch: watch::Watch,
    /// Round trips and write origins for leader placement (`placement.rs`).
    placement: placement::Placement,
    /// Linearizable reads in progress (`reads.rs`).
    reads: reads::Reads,
    /// Smoothed propose-to-commit latency of own proposals that were not hedged, in
    /// nanoseconds.
    commit_latency: Option<u64>,
    next_proposal: u64,
    next_alloc: u64,
    /// The slots this incarnation may lead at the top priority (`recovery.rs`).
    reservation: recovery::Reservation,

    // Compaction.
    /// First slot not covered by a snapshot; nothing below it is kept.
    floor: u64,
    /// Core state of the latest snapshot, carried into checkpoints.
    snapshot_state: Option<Bytes>,
    snapshot_asked: Option<Timestamp>,

    retirement: retire::Retirement,
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
    /// is durable. After a restart, that `Persist` also carries a catch-up request to every
    /// known replica, so one that was away learns what it missed without waiting for a
    /// write. See [`guide::durability`](crate::guide::durability).
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidConfig`] if `cfg` breaks a rule documented on [`Config`]; the
    ///   message says which.
    /// - [`Error::CorruptRecord`] if a record fails its checksum, does not decode, or holds
    ///   a value no further incarnation can follow (a slot past the last valid one, a
    ///   reservation through `u64::MAX`, incarnation `u64::MAX`); `index` is its position in
    ///   `durable`. Records are never skipped. Incarnation `u64::MAX` is also what a replica
    ///   restarted from `u64::MAX - 1` writes: after that, the counter is exhausted and
    ///   further restarts fail here, which is not corruption.
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
        // The next incarnation: one past the largest recorded, 1 on a fresh start.
        let mut incarnation = 1;
        let mut registers = BTreeMap::new();
        let mut log = BTreeMap::new();
        // The largest reservation, and the index of its record.
        let mut reservation: Option<(u64, usize)> = None;
        let mut snapshot: Option<(CoreState, Bytes)> = None;
        let cap = (cfg.max_batch_bytes as usize).saturating_mul(cfg.max_in_flight_slots as usize);
        let mut content = Store::new(cap);
        for (index, bytes) in durable.into_iter().enumerate() {
            match Record::decode(&bytes).map_err(|_| Error::CorruptRecord { index })? {
                Record::Incarnation(n) => {
                    let next = n.checked_add(1).ok_or(Error::CorruptRecord { index })?;
                    incarnation = incarnation.max(next);
                }
                Record::Recorder { slot, reg } => {
                    if reg.step > MAX_STEP || slot.0 > MAX_SLOT {
                        return Err(Error::CorruptRecord { index });
                    }
                    // Records are durable in order, and a register's content is written
                    // ahead of it: a reference without its `Data` record breaks promise 1.
                    for p in [&reg.first, &reg.agg, &reg.prev_agg].into_iter().flatten() {
                        content.pin_refs(&p.value, slot).map_err(|_| Error::CorruptRecord { index })?;
                    }
                    registers.insert(slot, reg);
                }
                Record::Reservation { through } => reservation = reservation.max(Some((through.0, index))),
                Record::Decided { slot, proposal } => {
                    if slot.0 > MAX_SLOT {
                        return Err(Error::CorruptRecord { index });
                    }
                    // A decision may lack its content (learned, not yet fetched), but not
                    // hold other bytes under a reference's id.
                    if content.pin_refs(&proposal.value, slot) == Err(PinError::Mismatch) {
                        return Err(Error::CorruptRecord { index });
                    }
                    log.insert(slot, Decision { proposal });
                }
                Record::Compacted { through, core_state } => {
                    let state = snapshot::decode_state(through, &core_state, &cfg.members)
                        .ok_or(Error::CorruptRecord { index })?;
                    if snapshot.as_ref().is_none_or(|(s, _)| s.next < state.next) {
                        snapshot = Some((state, core_state));
                    }
                }
                Record::Data { id, bytes } => content.restore(id, bytes),
            }
        }
        // A checkpoint built under another leader order would disagree on who leads.
        if snapshot.as_ref().is_some_and(|(s, _)| s.leaders != cfg.leaders()) {
            return Err(Error::InvalidConfig(
                "leader_order differs from the one this replica's durable state was built with",
            ));
        }
        let reservation = recovery::Reservation::recovered(reservation.map(|(t, _)| t))
            .ok_or(Error::CorruptRecord { index: reservation.map_or(0, |(_, i)| i) })?;
        let floor = snapshot.as_ref().map_or(0, |(s, _)| s.next);
        log = log.split_off(&Slot(floor));
        registers = registers.split_off(&Slot(floor));
        registers.retain(|slot, _| !log.contains_key(slot));
        content.raise_floor(floor);
        let wire = WireCtx {
            cluster_id: cfg.wire_cluster_id(),
            config_epoch: 0,
            max_message_bytes: cfg.max_message_bytes,
            max_value_bytes: cfg.max_batch_bytes,
        };
        let schedule = snapshot
            .as_ref()
            .map_or_else(|| Schedule::with_order(cfg.leaders()), |(s, _)| s.schedule.clone());
        let leader = schedule.leader(Slot(floor));
        let entries = entries::Entries::new(cfg.id, incarnation);
        let mut r = Self {
            me: cfg.id,
            wire,
            rng: ChaCha20::new(cfg.seed),
            schedule: schedule.clone(),
            applied_schedule: schedule,
            cfg,
            incarnation,
            now,
            phase: recovery::Phase::start(incarnation > 1, now),
            metrics: Metrics::default(),
            gate: Gate::new(Record::Incarnation(incarnation).encode()),
            actions: Vec::new(),
            inbox: VecDeque::new(),
            registers,
            log,
            committed: floor,
            emitted: floor,
            seen: BTreeMap::new(),
            unresolved: BTreeMap::new(),
            catch_up: recovery::CatchUp::default(),
            dedup: snapshot.as_ref().map_or_else(Dedup::default, |(s, _)| s.dedup.clone()),
            content,
            proposers: BTreeMap::new(),
            entries,
            watch: watch::Watch::default(),
            placement: placement::Placement::new(now, leader),
            reads: reads::Reads::default(),
            commit_latency: None,
            next_proposal: 0,
            next_alloc: 0,
            reservation,
            floor,
            snapshot_state: snapshot.map(|(_, b)| b),
            snapshot_asked: None,
            retirement: retire::Retirement::default(),
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
        if r.cfg.placement {
            r.restart_hold();
        }
        // After a restart, ask every known replica where the log is: one that was away, even
        // through its own removal, learns what it missed without waiting for someone to write.
        if incarnation > 1 {
            r.probe_peers();
        }
        // Nothing leaves this replica until its new incarnation is durable.
        r.flush();
        Ok(r)
    }

    /// This replica's id.
    pub fn id(&self) -> NodeId {
        self.me
    }

    /// This start's incarnation, advanced on every recovery from durable records.
    ///
    /// Match it with [`Entry::incarnation`] and [`Replica::id`] when associating committed
    /// entries with current client requests: an older start may have used the same proposal id.
    pub fn incarnation(&self) -> u64 {
        self.incarnation
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

    /// The members of the epoch in force at the committed prefix, in leader order (the
    /// leader first), with what this replica knows of each.
    pub fn members(&self) -> Vec<MemberView> {
        self.schedule
            .members(Slot(self.committed))
            .iter()
            .map(|&id| MemberView {
                id,
                round_trip: self.fresh_rtt(id),
                committed: if id == self.me {
                    Some(self.committed)
                } else {
                    self.watch.committed.get(&id).copied()
                },
                last_heard: self.watch.heard.get(&id).copied(),
            })
            .collect()
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
    /// - [`Error::RequestIdExhausted`] after this incarnation used every logged request id.
    pub fn propose(&mut self, now: Timestamp, payload: Bytes) -> Result<ProposalId, Error> {
        self.enqueue(now, payload, Kind::App)
    }

    /// Requests a linearizable read point.
    ///
    /// The returned id later appears in exactly one of [`Action::ReadReady`] or
    /// [`Action::Dropped`]: once the application has applied every commit up to the
    /// reported slot, reading its state reflects every write that completed before this
    /// call, on any replica. The read asks a majority for a bound and waits to apply up to
    /// it; it takes no slot and writes nothing. See [`guide::reads`](crate::guide::reads).
    ///
    /// # Errors
    ///
    /// [`Error::Busy`] (reads count against [`Config::max_outstanding_proposals`] with
    /// proposals), [`Error::Fenced`], [`Error::NotDrained`], [`Error::RequestIdExhausted`].
    pub fn read_barrier(&mut self, now: Timestamp) -> Result<ProposalId, Error> {
        self.begin(now)?;
        let id = self.submit_read()?;
        self.pump();
        self.flush();
        Ok(id)
    }

    /// Proposes new membership: `members`, with `leader_order` as the preferred leaders
    /// (empty: by id).
    ///
    /// Once decided in slot `s` it takes effect at slot `s + 64`, reported by
    /// [`Action::Reconfigured`] in log order; the id is reported like a proposal's. If
    /// nothing else is written, the slots up to it are filled with empty batches (by the
    /// leader after a hedge delay, or another member if it is down), so a change completes
    /// without traffic. A
    /// replica joining the cluster runs with the original [`Config`] (its own id need not
    /// be a member) and catches up before its first slot. See
    /// [`guide::membership`](crate::guide::membership).
    ///
    /// A change carries the generation of the leader orders it was computed from. If
    /// another change or a leader rotation is decided first, every replica ignores it and
    /// its id is reported as [`Action::Dropped`] with [`DropReason::Superseded`].
    /// [`DropReason::Unresolved`] means this replica learned it from a snapshot and cannot
    /// tell.
    ///
    /// # Errors
    ///
    /// - [`Error::ReconfigurationPending`] while a change (decided, or proposed here) has
    ///   not taken effect.
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
        let order = if !leader_order.is_empty() {
            leader_order
        } else if self.cfg.placement {
            // Placement owns the order: members that stay are ranked by cost (as last
            // scheduled for ties and for those without one, a rotation not yet in force
            // included), so the change itself puts the best-placed member first; new ones,
            // never measured, go last.
            let in_force = self.schedule.member_sets_from(Slot(self.committed)).last().unwrap_or_default();
            self.placed_order(in_force, &by_id)
        } else {
            by_id.clone()
        };
        let mut sorted = order.clone();
        sorted.sort_unstable();
        if sorted != by_id {
            return Err(Error::InvalidConfig("leader_order must list every member exactly once"));
        }
        self.check_no_change_pending()?;
        let change = crate::schedule::encode_change(self.schedule.generation(), &order);
        self.enqueue(now, change, Kind::Reconfigure)
    }

    /// Proposes a new leader order for the members in force: `order` lists each of them
    /// once, the preferred leader first. A membership change that keeps the members, decided
    /// and reported like [`Replica::propose_reconfiguration`] (in C, call
    /// `qpx_propose_reconfiguration` with the current members and the new order).
    ///
    /// # Errors
    ///
    /// - [`Error::ReconfigurationPending`] while a change (decided, or proposed here) has
    ///   not taken effect.
    /// - [`Error::InvalidConfig`] if `order` is not a permutation of the members in force.
    /// - As [`Replica::propose`] otherwise.
    pub fn propose_leader_order(&mut self, now: Timestamp, order: Vec<NodeId>) -> Result<ProposalId, Error> {
        self.check_no_change_pending()?;
        let mut in_force = self.schedule.members(Slot(self.committed)).to_vec();
        in_force.sort_unstable();
        let mut given = order.clone();
        given.sort_unstable();
        if given != in_force {
            return Err(Error::InvalidConfig("order must list every member in force exactly once"));
        }
        self.propose_reconfiguration(now, order.clone(), order)
    }

    fn check_no_change_pending(&self) -> Result<(), Error> {
        if self.schedule.epoch().1 > self.committed || self.entries.proposing_change() {
            return Err(Error::ReconfigurationPending);
        }
        Ok(())
    }

    fn enqueue(&mut self, now: Timestamp, payload: Bytes, kind: Kind) -> Result<ProposalId, Error> {
        self.begin(now)?;
        let id = self.submit(payload, kind)?;
        self.pump();
        self.flush();
        Ok(id)
    }

    /// Queues a local entry; the caller pumps it.
    fn submit(&mut self, payload: Bytes, kind: Kind) -> Result<ProposalId, Error> {
        if self.phase.fenced() {
            return Err(Error::Fenced);
        }
        if self.entries.own_len() + self.reads.len() >= self.cfg.max_outstanding_proposals as usize {
            return Err(Error::Busy);
        }
        if self.next_proposal >= READ_ID_BASE {
            return Err(Error::RequestIdExhausted);
        }
        let id = ProposalId(self.next_proposal);
        let item =
            Item { entry: Entry { origin: self.me, incarnation: self.incarnation, id, payload }, kind };
        if !batch::fits_alone(&item, self.cfg.max_batch_bytes as usize) {
            return Err(Error::PayloadTooLarge);
        }
        self.next_proposal += 1;
        self.entries.submit(self.now, item);
        Ok(id)
    }

    /// Feeds one external event: a message from a peer, or the completion or failure of
    /// a [`Action::Persist`].
    ///
    /// Malformed messages, messages from other clusters, and replies or decisions from
    /// replicas outside every known epoch are counted in [`Metrics::messages_rejected`] and
    /// otherwise ignored. Requests (to record, to catch up, forwarded entries) are served for
    /// any replica of the cluster: a removed replica whose epochs were all pruned still
    /// needs them to retire. After a storage failure the
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
        if self.phase.fenced() {
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
        if self.phase.fenced() {
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
        if self.phase.fenced() { None } else { self.earliest_timer() }
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

    /// Not a member of the latest epoch this replica knows: joining, removed, or added back
    /// by a change it has not learned yet.
    fn outside(&self) -> bool {
        !self.schedule.members(Slot(self.schedule.epoch().1)).contains(&self.me)
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
        let applied = Slot(self.emitted);
        let (epoch, from) = self.applied_schedule.epoch();
        self.metrics.epoch = if from <= applied.0 { epoch } else { epoch - 1 };
        self.metrics.leader_position = self.applied_schedule.position(self.me, applied);
        let before = self.gate.issued();
        self.gate.flush(&mut self.actions);
        if self.gate.issued() > before {
            self.watch.disk.issued(before, self.now);
        }
    }

    fn on_persisted(&mut self, id: PersistId) -> Result<(), Error> {
        let effects = self.gate.persisted(id, &mut self.actions)?;
        let d = self.hedge_delay();
        self.watch.disk.completed(id.0, self.now, d);
        for e in effects {
            self.apply(e);
        }
        Ok(())
    }

    fn apply(&mut self, e: Effect) {
        match e {
            Effect::ReservationDurable(t) => self.reservation.made_durable(t),
            // The recorder state is only garbage once the decision is durable here.
            Effect::DecidedDurable(slot) => {
                self.registers.remove(&slot);
            }
            Effect::LocalReply(body) => self.inbox.push_back((self.me, body)),
        }
    }

    fn fence(&mut self) {
        self.phase.fence();
        let moving = self.placement.policy.moving();
        if let Some(id) = moving {
            self.placement.policy.on_resolved(id);
        }
        let internal = [self.retirement.fence(), moving];
        for id in self.entries.fence().into_iter().map(|l| l.id).filter(|id| !internal.contains(&Some(*id))) {
            self.actions.push(Action::Dropped { id, reason: DropReason::Fenced });
        }
        self.drop_reads();
        self.proposers.clear();
        self.unresolved.clear();
        self.inbox.clear();
        self.gate.fence();
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
            local_batches: self.entries.batch_count(),
            entries: self.entry_probes(),
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
mod content_tests;
#[cfg(test)]
mod decided_tests;
#[cfg(test)]
mod disk_tests;
#[cfg(test)]
mod egress_tests;
#[cfg(test)]
mod placement_tests;
#[cfg(test)]
mod proposing_tests;
#[cfg(test)]
mod ranking_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod watch_tests;

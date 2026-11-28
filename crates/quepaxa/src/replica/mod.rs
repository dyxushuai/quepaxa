//! The sans-IO replica: one proposer and one recorder per slot, a decided log, and the
//! durability gate that keeps replies behind the writes they depend on (ADR 0002).
//!
//! `gate` holds the durability gate, `log` the recorder and decided-log side, `messages`
//! sending and receiving, `recovery` the way from a start to normal operation (the phase,
//! catching up, the leader's reservation), `proposing` the proposer side (batching,
//! forwarding to the leader, hedging, stalled slots), `entries` where entries wait until
//! they commit.

mod api;
mod content;
mod disk;
mod entries;
mod forwards;
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

use crate::api::{Action, Config, DropReason, Error, Metrics, Outbound};
#[cfg(test)]
use crate::api::{Input, MemberView};
#[cfg(test)]
use crate::batch::{self, Entry, Item, Kind, Piece};
use crate::codec::{Body, Envelope, Message, WireCtx};
use crate::content::{PinError, Store};
use crate::core_state::CoreState;
use crate::dedup::Dedup;
use crate::durable::Record;
use crate::proposal::{Proposal, ValueKey};
use crate::proposer::SlotProposer;
use crate::recorder::Register;
use crate::rng::ChaCha20;
use crate::schedule::Schedule;
#[cfg(test)]
use crate::types::ProposalId;
use crate::types::{MAX_SLOT, MAX_STEP, NodeId, PersistId, Slot, Timestamp};

#[cfg_attr(feature = "model", derive(Clone, Hash))]
struct Decision {
    proposal: Proposal,
}

#[cfg_attr(feature = "model", derive(Clone, Hash))]
struct Active {
    prop: SlotProposer,
    batch: u64,
    resend_at: Timestamp,
    /// When the current step's requests were last sent.
    sent_at: Timestamp,
    /// When this replica started proposing in the slot.
    started: Timestamp,
    /// Rule P: waiting for referenced content before sending this step's record requests.
    waiting_content: bool,
    /// Recorder that reported the currently adopted working value, if any.
    holder: Option<NodeId>,
    /// Recorders that reported candidate values in the current step's replies.
    holders: BTreeMap<ValueKey, NodeId>,
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
    /// Contiguous decided prefix whose referenced content is all held and written to the
    /// durable log; used for catch-up and retirement progress (#172).
    held: u64,
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
    /// Content dissemination, rule-R parking, and fetch state (`replica/content.rs`).
    fetching: content::Fetching,

    // Proposer side.
    /// Active in-flight slot limit (`1..=cfg.max_in_flight_slots`), movable at runtime
    /// without shrinking the content store or peer-acceptance windows (#251).
    in_flight_slots: u32,
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
    /// Smoothed time from starting a slot to its decision at the first step, over slots
    /// whose requests were never sent again, in nanoseconds (`send_requests`).
    request_rtt: Option<u64>,
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
        let mut decided_at = BTreeMap::new();
        // The largest reservation, and the index of its record.
        let mut reservation: Option<(u64, usize)> = None;
        let mut snapshot: Option<(CoreState, Bytes)> = None;
        let mut content = Store::new(cfg.content_cache_cap());
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
                    decided_at.insert(slot, index);
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
        // A `Decided` record may precede the `Data` record fetched for it: pin again now that
        // every `Data` record has been restored, before `raise_floor` trims unpinned batches.
        let mut missing_decided = Vec::new();
        for (&slot, d) in &log {
            match content.pin_refs(&d.proposal.value, slot) {
                Ok(()) => {}
                Err(PinError::Missing) => missing_decided.push(slot),
                Err(PinError::Mismatch) => {
                    return Err(Error::CorruptRecord { index: decided_at[&slot] });
                }
            }
        }
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
        let in_flight_slots = cfg.max_in_flight_slots;
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
            held: floor,
            emitted: floor,
            seen: BTreeMap::new(),
            unresolved: BTreeMap::new(),
            catch_up: recovery::CatchUp::default(),
            dedup: snapshot.as_ref().map_or_else(Dedup::default, |(s, _)| s.dedup.clone()),
            content,
            fetching: content::Fetching::default(),
            in_flight_slots,
            proposers: BTreeMap::new(),
            entries,
            watch: watch::Watch::default(),
            placement: placement::Placement::new(now, leader),
            reads: reads::Reads::default(),
            commit_latency: None,
            request_rtt: None,
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
        for slot in missing_decided {
            r.note_missing_slot(slot, None);
            if let Some(d) = r.log.get(&slot) {
                let origin = d.proposal.key.origin;
                let value = d.proposal.value.clone();
                if let Ok(missing) = r.check_refs(&value)
                    && !missing.is_empty()
                {
                    r.fetch_refs(slot, origin, &missing);
                }
            }
        }
        r.advance_held();
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

    /// Not a member of the latest epoch this replica knows: joining, removed, or added back
    /// by a change it has not learned yet.
    fn outside(&self) -> bool {
        !self.schedule.members(Slot(self.schedule.epoch().1)).contains(&self.me)
    }

    /// Configured maximum window (`cfg.max_in_flight_slots`): bounds peer acceptance,
    /// reservation horizons and catch-up.
    fn window(&self) -> u64 {
        u64::from(self.cfg.max_in_flight_slots)
    }

    /// Active driving window (`in_flight_slots`): how many slots this replica drives at once.
    fn active_window(&self) -> u64 {
        u64::from(self.in_flight_slots)
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
        self.watch.fwd.clear();
        for id in self.entries.fence().into_iter().map(|l| l.id).filter(|id| !internal.contains(&Some(*id))) {
            self.actions.push(Action::Dropped { id, reason: DropReason::Fenced });
        }
        self.drop_reads();
        self.proposers.clear();
        self.unresolved.clear();
        self.fence_content();
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
mod rotation_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod watch_tests;

//! The sans-IO replica: one proposer and one recorder per slot, a decided log, and the
//! durability gate that keeps replies behind the writes they depend on (ADR 0002).
//!
//! `log` holds the recorder and decided-log side, `proposing` the proposer side (batching,
//! forwarding to the leader, hedging, stalled slots).

mod log;
mod proposing;
mod snapshot;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use bytes::Bytes;
use core::mem;

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
    Ready,
    ReservationDurable(u64),
    DecidedDurable(Slot),
    LocalReply(Body),
}

#[cfg_attr(feature = "model", derive(Clone, Hash))]
struct InFlight {
    id: u64,
    effects: Vec<Effect>,
    parked: Vec<Outbound>,
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
    /// The next `Persist` replaces all earlier records.
    replace_next: bool,
    snapshot_asked: Option<Timestamp>,
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
                    let state = CoreState::decode(&core_state, &cfg.members)
                        .map_err(|_| Error::CorruptRecord { index })?;
                    if through.0.checked_add(1) != Some(state.next) || state.next > MAX_SLOT {
                        return Err(Error::CorruptRecord { index });
                    }
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
            cluster_id: cfg.cluster_id,
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
                .map_or_else(|| Schedule::new(&cfg.members), |(s, _)| s.schedule.clone()),
            applied_schedule: snapshot
                .as_ref()
                .map_or_else(|| Schedule::new(&cfg.members), |(s, _)| s.schedule.clone()),
            cfg,
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
            replace_next: false,
            snapshot_asked: None,
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
        if 4 + batch::encoded_len(&item) > self.cfg.max_batch_bytes as usize {
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
        self.metrics.retained_slots = self.log.len() as u64;
        self.metrics.live_registers = self.registers.len() as u64;
        if self.writes.is_empty() {
            if let Some(last) = self.in_flight.back_mut() {
                last.effects.append(&mut self.effects);
                last.parked.append(&mut self.gated);
            }
            debug_assert!(self.gated.is_empty() && self.effects.is_empty(), "gated work without a write");
            return;
        }
        let id = self.next_persist;
        self.next_persist += 1;
        self.actions.push(Action::Persist {
            id: PersistId(id),
            records: mem::take(&mut self.writes),
            then: mem::take(&mut self.gated),
            replace: mem::take(&mut self.replace_next),
        });
        self.in_flight.push_back(InFlight { id, effects: mem::take(&mut self.effects), parked: Vec::new() });
    }

    fn on_persisted(&mut self, PersistId(id): PersistId) -> Result<(), Error> {
        if id >= self.next_persist || self.acked.is_some_and(|a| id <= a) {
            return Err(Error::UnknownPersistId);
        }
        self.acked = Some(id);
        while self.in_flight.front().is_some_and(|f| f.id <= id) {
            let Some(done) = self.in_flight.pop_front() else { break };
            self.actions.extend(done.parked.into_iter().map(Action::Send));
            for e in done.effects {
                self.apply(e);
            }
        }
        Ok(())
    }

    fn apply(&mut self, e: Effect) {
        match e {
            Effect::Ready => self.ready = true,
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
        Outbound { to, bytes: Message { env, body }.encode() }
    }

    /// A send that does not depend on durable state.
    fn send(&mut self, to: NodeId, body: Body) {
        if to == self.me {
            self.inbox.push_back((to, body));
            return;
        }
        let out = self.encode(to, body);
        if self.ready {
            self.actions.push(Action::Send(out));
        } else {
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
mod tests {
    use super::*;
    use crate::codec::reply_proposal;
    use proptest::prelude::*;

    fn replica() -> Replica {
        let cfg = Config::new([3; 16], NodeId(0), alloc::vec![NodeId(0), NodeId(1), NodeId(2)], [9; 32]);
        let mut r = Replica::new(cfg, [], Timestamp(0)).unwrap();
        let mut out = Vec::new();
        r.poll_actions(&mut out);
        r.handle(Timestamp(0), Input::Persisted(PersistId(0))).unwrap();
        r.poll_actions(&mut out);
        r
    }

    fn proposal(priority: u64, origin: u32, seq: u64) -> Proposal {
        Proposal {
            priority,
            key: ValueKey { origin: NodeId(origin), incarnation: 1, seq },
            value: batch::encode(&[]),
        }
    }

    fn arb_body() -> impl Strategy<Value = Body> {
        let p = (any::<u64>(), 0u32..4, 0u64..3).prop_map(|(pr, o, s)| proposal(pr, o, s));
        let slot = prop_oneof![0u64..4, Just(u64::MAX), any::<u64>()].prop_map(Slot);
        let step = prop_oneof![0u32..12, Just(u32::MAX), any::<u32>()];
        prop_oneof![
            (slot.clone(), step.clone(), p.clone()).prop_map(|(slot, step, proposal)| Body::RecordRequest {
                slot,
                step,
                proposal
            }),
            (
                slot.clone(),
                step.clone(),
                step,
                proptest::option::of(p.clone()),
                proptest::option::of(p.clone())
            )
                .prop_map(|(slot, req_step, step, f, a)| {
                    let key = ValueKey { origin: NodeId(0), incarnation: 1, seq: 0 };
                    Body::RecordReply {
                        slot,
                        req_step,
                        step,
                        first: f.map(|f| reply_proposal(&f, key)),
                        prev_agg: a.map(|a| reply_proposal(&a, key)),
                    }
                }),
            (slot.clone(), p.clone()).prop_map(|(slot, proposal)| Body::Decided { slot, proposal }),
            slot.clone().prop_map(|from| Body::CatchUp { from }),
            (slot.clone(), proptest::collection::vec(p, 0..3))
                .prop_map(|(first, proposals)| Body::Entries { first, proposals }),
            slot.prop_map(|through| Body::Compacted { through }),
        ]
    }

    #[test]
    fn lost_catch_up_request_is_retried_on_the_timer() {
        let mut r = replica();
        let mut out = Vec::new();
        let catch_ups = |out: &[Action]| {
            out.iter()
                .filter(|a| match a {
                    Action::Send(o) => {
                        matches!(
                            Message::decode(&o.bytes, &r_wire()),
                            Ok(Message { body: Body::CatchUp { .. }, .. })
                        )
                    }
                    _ => false,
                })
                .count()
        };
        // A peer advertises ten decided slots; the request it triggers is lost.
        let env = Envelope {
            cluster_id: [3; 16],
            config_epoch: 0,
            from: NodeId(1),
            to: NodeId(0),
            commit_index: 10,
        };
        let msg = Message { env, body: Body::CatchUp { from: Slot(0) } }.encode();
        r.handle(Timestamp(1), Input::Message(msg)).unwrap();
        r.poll_actions(&mut out);
        assert_eq!(catch_ups(&out), 1);
        out.clear();
        let deadline = r.next_deadline().expect("a retry is scheduled");
        r.handle_timeout(deadline).unwrap();
        r.poll_actions(&mut out);
        assert_eq!(catch_ups(&out), 1, "{out:?}");
        assert_eq!(r.metrics().catch_up_retries, 1);
    }

    fn r_wire() -> WireCtx {
        WireCtx {
            cluster_id: [3; 16],
            config_epoch: 0,
            max_message_bytes: 16 << 20,
            max_value_bytes: 1 << 20,
        }
    }

    #[test]
    fn core_state_schedule_covers_only_emitted_slots() {
        let mut r = replica();
        let mut out = Vec::new();
        // Ten decisions not originated by the leader (node 0) would rotate it, but they are
        // not durable yet, so nothing is emitted and core state must not reflect them.
        for slot in 0..10 {
            let env = Envelope {
                cluster_id: [3; 16],
                config_epoch: 0,
                from: NodeId(1),
                to: NodeId(0),
                commit_index: 0,
            };
            let body = Body::Decided { slot: Slot(slot), proposal: proposal(1, 1, slot) };
            r.handle(Timestamp(1), Input::Message(Message { env, body }.encode())).unwrap();
            r.poll_actions(&mut out);
        }
        assert_eq!(r.emitted_index(), 0);
        let fresh = Schedule::new(&[NodeId(0), NodeId(1), NodeId(2)]);
        let state = CoreState::decode(&r.core_state(), &r.cfg.members).unwrap();
        assert_eq!(state.schedule, fresh);
        assert_ne!(r.schedule, fresh, "the driving schedule already saw the decided prefix");
        let last = out.iter().rev().find_map(|a| match a {
            Action::Persist { id, .. } => Some(*id),
            _ => None,
        });
        r.handle(Timestamp(2), Input::Persisted(last.unwrap())).unwrap();
        r.poll_actions(&mut out);
        assert_eq!(r.emitted_index(), 10);
        let state = CoreState::decode(&r.core_state(), &r.cfg.members).unwrap();
        assert_eq!(state.schedule, r.schedule);
    }

    proptest! {
        // Well-formed messages with adversarial field values from members never panic or
        // violate the API contract.
        #[test]
        fn arbitrary_member_messages_are_handled(msgs in proptest::collection::vec((1u32..3, arb_body()), 1..40)) {
            let mut r = replica();
            r.propose(Timestamp(1), Bytes::from_static(b"x")).unwrap();
            let mut out = Vec::new();
            r.poll_actions(&mut out);
            for (i, (from, body)) in msgs.into_iter().enumerate() {
                let env = Envelope { cluster_id: [3; 16], config_epoch: 0, from: NodeId(from), to: NodeId(0), commit_index: i as u64 };
                let bytes = Message { env, body }.encode();
                prop_assert!(r.handle(Timestamp(2 + i as u64), Input::Message(bytes)).is_ok());
                r.poll_actions(&mut out);
                let _ = r.handle_timeout(Timestamp(1_000_000_000 * (i as u64 + 1)));
                r.poll_actions(&mut out);
            }
        }
    }
}

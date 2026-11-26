//! Public driving and query methods of [`Replica`]: proposals, reads, membership changes,
//! event feeding, timers and action draining.

use alloc::vec::Vec;
use bytes::Bytes;

use super::Replica;
use crate::api::{Action, Error, Input, MemberView, Metrics};
use crate::batch::{self, Entry, Item, Kind};
use crate::types::{NodeId, ProposalId, READ_ID_BASE, Slot, Timestamp};

impl Replica {
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

    /// Number of slots this replica currently drives concurrently (`1..=cfg.max_in_flight_slots`).
    pub fn in_flight_slots(&self) -> u32 {
        self.in_flight_slots
    }

    /// Sets how many slots this replica drives concurrently, within `1..=cfg.max_in_flight_slots`.
    ///
    /// Takes effect on the next call that drives slots ([`propose`](Self::propose),
    /// [`handle`](Self::handle), [`handle_timeout`](Self::handle_timeout)). Slots already in
    /// flight finish normally; the content store, peer-acceptance windows and reservation
    /// horizons keep their configured bounds and do not shrink or evict.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidConfig`] if `slots` is `0` or exceeds `Config::max_in_flight_slots`.
    pub fn set_in_flight_slots(&mut self, slots: u32) -> Result<(), Error> {
        if slots == 0 {
            return Err(Error::InvalidConfig("in_flight_slots must be positive"));
        }
        if slots > self.cfg.max_in_flight_slots {
            return Err(Error::InvalidConfig("in_flight_slots must not exceed max_in_flight_slots"));
        }
        self.in_flight_slots = slots;
        Ok(())
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
    /// - [`Error::Busy`] when [`Config::max_outstanding_proposals`](crate::Config::max_outstanding_proposals)
    ///   entries are already waiting: back off and retry after some commit.
    /// - [`Error::PayloadTooLarge`] if the entry alone exceeds
    ///   [`Config::max_batch_bytes`](crate::Config::max_batch_bytes).
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
    /// [`Error::Busy`] (reads count against
    /// [`Config::max_outstanding_proposals`](crate::Config::max_outstanding_proposals) with
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
    /// replica joining the cluster runs with the original [`Config`](crate::Config) (its own
    /// id need not be a member) and catches up before its first slot. See
    /// [`guide::membership`](crate::guide::membership).
    ///
    /// A change carries the generation of the leader orders it was computed from. If
    /// another change or a leader rotation is decided first, every replica ignores it and
    /// its id is reported as [`Action::Dropped`] with [`DropReason::Superseded`](crate::DropReason::Superseded).
    /// [`DropReason::Unresolved`](crate::DropReason::Unresolved) means this replica learned
    /// it from a snapshot and cannot tell.
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
    pub(super) fn submit(&mut self, payload: Bytes, kind: Kind) -> Result<ProposalId, Error> {
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

    pub(super) fn begin(&mut self, now: Timestamp) -> Result<(), Error> {
        if !self.actions.is_empty() {
            return Err(Error::NotDrained);
        }
        self.now = self.now.max(now);
        Ok(())
    }
}

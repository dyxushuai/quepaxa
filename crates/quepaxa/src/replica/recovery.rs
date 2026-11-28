//! Recovery: a replica's way from a start to normal operation, catching up with peers, and
//! the reservation that keeps a restart from reusing the leader priority.
//!
//! Three pieces of state, kept apart because each combination of them is reachable (#200):
//!
//! - [`Phase`]: `Recovering → Live`, and any phase `→ Fenced`. A replica restarted from
//!   durable records with someone to ask starts `Recovering`: it asks every known replica
//!   where the log is, on the retransmit timer, until a majority of the members at its decided
//!   prefix (counting itself) has answered. Others start `Live`. A storage failure fences it
//!   for good: it then ignores every input.
//! - [`CatchUp`]: the most advanced commit index a peer advertised, and the bulk request in
//!   flight. A replica falls behind and catches up in any phase, and a request is still
//!   outstanding when an unanswered peer's slots arrive another way, so catching up is not a
//!   phase: it is read from the commit indexes (and, outside the membership, from the read
//!   mark) every time.
//! - [`Reservation`]: the slots this incarnation may lead at the top priority. Set once from
//!   the recovered records and extended by the leader; it does not depend on the phase (a
//!   restarted leader may lead while it is still recovering, past what it reserved before).
//!
//! Proved on a port in `verification/verus/recovery.rs`.

use alloc::collections::BTreeSet;

use super::Replica;
use crate::codec::Body;
use crate::types::{NodeId, Slot, Timestamp};

/// Where this replica is between its start and its end.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) enum Phase {
    /// Restarted: asking every known replica where the log is (again at `probe_at`) until a
    /// majority of the members has answered, not just one: one as far behind as this
    /// replica tells it nothing.
    Recovering { probe_at: Timestamp, answered: BTreeSet<NodeId> },
    /// Started fresh, alone, or answered by a majority.
    Live,
    /// A storage failure: nothing happens any more.
    Fenced,
}

impl Phase {
    /// `Recovering` after a restart (until `probed` says when to ask again, or that there is
    /// nobody to ask), `Live` otherwise.
    pub(super) fn start(restarted: bool, now: Timestamp) -> Self {
        if restarted { Self::Recovering { probe_at: now, answered: BTreeSet::new() } } else { Self::Live }
    }

    pub(super) fn fenced(&self) -> bool {
        *self == Self::Fenced
    }

    /// When to ask the replicas that have not answered again, while recovering.
    pub(super) fn probe_at(&self) -> Option<Timestamp> {
        match self {
            Self::Recovering { probe_at, .. } => Some(*probe_at),
            Self::Live | Self::Fenced => None,
        }
    }

    /// The replicas were asked while recovering: again at `at`, or never with nobody to ask.
    /// Answers already in count.
    pub(super) fn probed(&mut self, at: Option<Timestamp>) {
        let Self::Recovering { probe_at, .. } = self else { return };
        match at {
            Some(at) => *probe_at = at,
            None => *self = Self::Live,
        }
    }

    /// `from` answered with a catch-up page (empty or not).
    pub(super) fn answered(&mut self, from: NodeId) {
        if let Self::Recovering { answered, .. } = self {
            answered.insert(from);
        }
    }

    /// Recovered once a majority of `members`, counting `me`, answered.
    pub(super) fn check(&mut self, me: NodeId, members: &[NodeId]) {
        let Self::Recovering { answered, .. } = self else { return };
        let count = members.iter().filter(|m| **m == me || answered.contains(m)).count();
        if count > members.len() / 2 {
            *self = Self::Live;
        }
    }

    pub(super) fn fence(&mut self) {
        *self = Self::Fenced;
    }
}

/// How far peers are ahead, and the bulk catch-up request in flight.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct CatchUp {
    /// The largest commit index a replica of a known epoch advertised, and who did.
    ahead: u64,
    peer: Option<NodeId>,
    /// The outstanding request: whom it asked, and when.
    request: Option<(NodeId, Timestamp)>,
}

impl CatchUp {
    /// `from` advertised `commit_index` in an envelope.
    pub(super) fn heard(&mut self, from: NodeId, commit_index: u64) {
        if commit_index > self.ahead {
            self.ahead = commit_index;
            self.peer = Some(from);
        }
    }

    /// The largest commit index a replica of a known epoch advertised.
    pub(super) fn ahead(&self) -> u64 {
        self.ahead
    }

    /// A peer decided slots past `committed`.
    pub(super) fn ahead_of(&self, committed: u64) -> bool {
        self.ahead > committed
    }

    /// Worth a bulk catch-up request: for a member, more than the next slot is decided
    /// elsewhere (the next one alone comes with the traffic); for a replica outside the
    /// membership, nobody broadcasts `Decided` to it, so even one slot ahead is behind.
    pub(super) fn behind(&self, committed: u64, outside: bool) -> bool {
        if outside { self.ahead > committed } else { self.ahead > committed + 1 }
    }

    /// Whether a request may be (re)sent now.
    pub(super) fn due(&self, now: Timestamp, interval: u64) -> bool {
        self.request.is_none_or(|(_, t)| t.after(interval) <= now)
    }

    /// Whether a bulk catch-up request is currently in flight.
    pub(super) fn in_flight(&self) -> bool {
        self.request.is_some()
    }

    pub(super) fn requested(&mut self, peer: NodeId, now: Timestamp) {
        self.request = Some((peer, now));
    }

    /// Worth a bulk catch-up request when allowing `lag` in-flight slots to arrive via normal
    /// `RecordRequest` / `Decided` traffic.
    pub(super) fn behind_by(&self, committed: u64, lag: u64) -> bool {
        self.ahead > committed.saturating_add(lag)
    }

    /// A page from `from` was applied, bringing the prefix to `committed`. Only the answer
    /// to the request in flight moves it along: a page ends it, and returns whom to ask for
    /// the next one if still behind; an empty one leaves it to the timer.
    pub(super) fn page(
        &mut self,
        from: NodeId,
        empty: bool,
        committed: u64,
        outside: bool,
        now: Timestamp,
    ) -> Option<NodeId> {
        if !self.request.is_some_and(|(peer, _)| peer == from) {
            return None;
        }
        if empty {
            self.request = Some((from, now));
            return None;
        }
        self.request = None;
        self.behind(committed, outside).then_some(from)
    }

    /// When to retry a bulk request on the timer, while behind a peer. If within the active
    /// pipeline `window` while committing normally (`last_committed`), waits until `interval`
    /// after `last_committed` instead of firing immediately on in-flight WAN lag.
    pub(super) fn deadline_with(
        &self,
        committed: u64,
        outside: bool,
        window: u64,
        last_committed: Option<Timestamp>,
        now: Timestamp,
        interval: u64,
    ) -> Option<Timestamp> {
        if !self.behind(committed, outside) || self.request.or(self.peer.map(|p| (p, now))).is_none() {
            return None;
        }
        let immediate = outside || self.behind_by(committed, window);
        let timer = self.request.map_or(now, |(_, t)| t.after(interval));
        if immediate {
            Some(timer)
        } else {
            let quiet_at = last_committed.map_or(now, |t| t.after(interval));
            Some(timer.max(quiet_at))
        }
    }

    /// Whom to ask on the timer: while behind past `lag`, the most advanced peer (the
    /// previous target may be the one that failed); otherwise the peer `read_from` names.
    pub(super) fn retry_by(
        &self,
        committed: u64,
        lag: u64,
        now: Timestamp,
        interval: u64,
        read_from: impl FnOnce() -> Option<NodeId>,
    ) -> Option<NodeId> {
        if !self.due(now, interval) {
            return None;
        }
        if self.behind_by(committed, lag) { self.peer.or(self.request.map(|(p, _)| p)) } else { read_from() }
    }

    /// A snapshot answered any request in flight; whom to ask for what was decided after it.
    pub(super) fn installed(&mut self, committed: u64) -> Option<NodeId> {
        self.request = None;
        self.peer.filter(|_| self.ahead_of(committed))
    }

    #[cfg(test)]
    pub(super) fn deadline(
        &self,
        committed: u64,
        outside: bool,
        now: Timestamp,
        interval: u64,
    ) -> Option<Timestamp> {
        let behind =
            self.behind(committed, outside) && self.request.or(self.peer.map(|p| (p, now))).is_some();
        behind.then(|| self.request.map_or(now, |(_, t)| t.after(interval)))
    }

    #[cfg(test)]
    pub(super) fn retry(
        &self,
        committed: u64,
        outside: bool,
        now: Timestamp,
        interval: u64,
        read_from: impl FnOnce() -> Option<NodeId>,
    ) -> Option<NodeId> {
        self.retry_by(committed, if outside { 0 } else { 1 }, now, interval, read_from)
    }
}

/// The slots this incarnation may propose at the leader priority in.
///
/// A leader proposes at the top priority in its slots only past every reservation a previous
/// incarnation made durable, and a top-priority request leaves for another recorder only
/// once a reservation covering its slot is durable: a restart never proposes a second
/// top-priority value in a slot (the paper's fast path, §4.2.5, uses the top priority once).
#[derive(Clone, Debug)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct Reservation {
    /// First slot this incarnation may lead at the top priority.
    floor: u64,
    /// The latest reservation this incarnation wrote.
    through: Option<u64>,
    /// The latest of them known durable.
    durable: Option<u64>,
}

impl Reservation {
    /// The largest reservation in the recovered records, if any. `None` for one through
    /// `u64::MAX`: no slot lies past it, and the replica never writes it (it reserves
    /// `slot + 4 · window` with `slot` at most `MAX_SLOT`), so its record is corrupt.
    pub(super) fn recovered(through: Option<u64>) -> Option<Self> {
        let floor = through.map_or(Some(0), |r| r.checked_add(1))?;
        Some(Self { floor, through: None, durable: None })
    }

    /// Whether `slot` is past every reservation recovered.
    pub(super) fn may_lead(&self, slot: Slot) -> bool {
        slot.0 >= self.floor
    }

    /// Leading `slot` at the top priority: past the latest reservation, a new one through
    /// `slot + span`, to write before any request for it leaves.
    pub(super) fn extend(&mut self, slot: Slot, span: u64) -> Option<u64> {
        if self.through.is_some_and(|t| slot.0 <= t) {
            return None;
        }
        let through = slot.0 + span;
        self.through = Some(through);
        Some(through)
    }

    /// The reservation through `through` is durable.
    pub(super) fn made_durable(&mut self, through: u64) {
        self.durable = self.durable.max(Some(through));
    }

    /// Whether a durable reservation covers `slot`.
    pub(super) fn covers(&self, slot: Slot) -> bool {
        self.durable.is_some_and(|t| slot.0 <= t)
    }

    /// What a checkpoint must keep: the latest reservation made, by this incarnation or
    /// recovered.
    pub(super) fn checkpoint(&self) -> Option<u64> {
        self.through.or(self.floor.checked_sub(1))
    }
}

impl Replica {
    /// Asks every known replica where the log is (again on the retransmit timer, until a
    /// majority of the members answered; see `check_probe_answers`).
    pub(super) fn probe_peers(&mut self) {
        let peers: BTreeSet<NodeId> = self
            .cfg
            .members
            .iter()
            .copied()
            .chain(self.schedule.everyone())
            .filter(|&m| m != self.me)
            .collect();
        // Nobody to ask (a single member): nothing to wait for either.
        self.phase.probed((!peers.is_empty()).then(|| self.now.after(self.cfg.retransmit_interval)));
        for peer in peers {
            self.send(peer, Body::CatchUp { from: Slot(self.committed) });
        }
    }

    /// Ends the startup probe once a majority of the members at the decided prefix, counting
    /// this replica, answered it. Checked after a page is applied: a page that crosses into a
    /// new epoch must be answered by its members, not the old ones.
    pub(super) fn check_probe_answers(&mut self) {
        let members = self.schedule.members(Slot(self.committed));
        self.phase.check(self.me, members);
    }

    /// Active pipeline margin tolerated on a committing voting member before requesting bulk
    /// `CatchUp`: twice `max_in_flight_slots` covers in-flight pipelining plus one-way WAN
    /// `Decided` propagation skew without delaying recovery once commits stall.
    pub(super) fn catch_up_window(&self) -> u64 {
        self.window().saturating_mul(2)
    }

    /// How many slots of peer `commit_index` lead are tolerated as normal in-flight pipelining
    /// before triggering bulk `CatchUp` (`Body::Entries`): `0` outside membership,
    /// `catch_up_window()` while `committed` advanced within `retransmit_interval`, and `1`
    /// once quiet/stalled.
    pub(super) fn catch_up_lag(&self) -> u64 {
        if self.outside() {
            return 0;
        }
        if self.last_committed_at.is_some_and(|t| self.now < t.after(self.cfg.retransmit_interval)) {
            self.catch_up_window()
        } else {
            1
        }
    }

    /// Whether a bulk catch-up request may be (re)sent now.
    pub(super) fn catch_up_due(&self) -> bool {
        self.catch_up.due(self.now, self.cfg.retransmit_interval)
    }

    /// Fetches the decided prefix from `committed` in bulk.
    pub(super) fn request_catch_up(&mut self, peer: NodeId) {
        self.catch_up.requested(peer, self.now);
        self.send(peer, Body::CatchUp { from: Slot(self.committed) });
    }
}

#[cfg(test)]
mod tests;

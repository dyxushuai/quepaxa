//! Leader placement's decisions as one state machine, apart from the replica: who leads and
//! since when (`Tenure`), what placement is doing about it (`Pending`), when the order last
//! changed, which members rotation demoted, and the writers' tail measured under each
//! leader. `replica/placement.rs` and `replica/ranking.rs` gather the round trips, rates
//! and costs and feed them in; nothing here reads the replica. `verification/verus/placement.rs`
//! is its port: a change here must be ported there.
//!
//! The state changes only on: an order change taking effect (`on_order_applied`), a
//! restart or snapshot (`on_restart`), a probe round (`sample`, then `decide` on the
//! leader), and placement's own move being submitted and resolved (`on_submitted`,
//! `on_resolved`).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::types::{NodeId, ProposalId, Timestamp};

/// How often a member probes while writes flow: one round.
pub(crate) const PROBE_INTERVAL: u64 = 1_000_000_000;
/// Rounds of samples a tenure needs before it is compared.
const MIN_ROUNDS: u64 = 10;
/// Holds after which a measurement no longer describes its member.
const STALE_HOLDS: u64 = 8;
/// Members whose cost is within this fraction (tenths) of the leader's, plus `TIE_SLACK`,
/// rank alike.
const TIE: u128 = 11;
/// Round-trip cost per write below which members rank alike: 5 ms.
const TIE_SLACK: u128 = 5_000_000;
/// Fewest smoothed writes per round (rates are eight times the per-round count) for a move:
/// about one write a second.
pub(crate) const WRITE_FLOOR: u64 = 8;

/// `Config::placement_sustain` and `Config::placement_hold`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Periods {
    pub(crate) sustain: u64,
    pub(crate) hold: u64,
}

/// The leader in force as this replica saw it start. Built only from a `now` the replica
/// was given, so a tenure never starts at the clock's zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(crate) struct Tenure {
    leader: NodeId,
    since: Timestamp,
}

#[cfg(test)]
impl Tenure {
    pub(crate) fn leader(&self) -> NodeId {
        self.leader
    }

    pub(crate) fn since(&self) -> Timestamp {
        self.since
    }
}

/// What placement is doing about the leader in force: nothing, keeping track of a member
/// that keeps a gain over it, or waiting for its own order change to resolve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(crate) enum Pending {
    Idle,
    Tracking(Target),
    Moving(ProposalId),
}

/// The member that has kept a gain over `leader`, and since when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(crate) struct Target {
    pub(crate) member: NodeId,
    pub(crate) leader: NodeId,
    pub(crate) since: Timestamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(crate) struct Exclusion {
    pub(crate) until: Timestamp,
    /// Holds this exclusion lasts: doubled each time the member is demoted again before it
    /// led for a whole hold.
    pub(crate) factor: u64,
}

/// Per-round samples (microseconds): count, sum, sum of squares, and the first and latest
/// rounds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(crate) struct Led {
    pub(crate) count: u64,
    pub(crate) sum: u128,
    pub(crate) sum_sq: u128,
    pub(crate) since: Timestamp,
    pub(crate) at: Timestamp,
}

impl Led {
    pub(crate) fn add(&mut self, us: u64, at: Timestamp) {
        if self.count == 0 {
            self.since = at;
        }
        self.count = self.count.saturating_add(1);
        self.sum = self.sum.saturating_add(u128::from(us));
        self.sum_sq = self.sum_sq.saturating_add(u128::from(us) * u128::from(us));
        self.at = at;
    }

    pub(crate) fn mean(&self) -> u128 {
        self.sum / u128::from(self.count.max(1))
    }

    /// Variance of the mean.
    fn var_of_mean(&self) -> u128 {
        let n = u128::from(self.count.max(1));
        self.sum_sq.saturating_sub(self.sum.saturating_mul(self.sum) / n) / n / n
    }

    /// Whether these samples are slower than `other`'s by more than three standard errors.
    fn worse_than(&self, other: &Led) -> bool {
        let (a, b) = (self.mean(), other.mean());
        a > b
            && (a - b).saturating_mul(a - b)
                > 9u128.saturating_mul(self.var_of_mean().saturating_add(other.var_of_mean()))
    }

    /// Samples over a whole sustained period (sampling starts two rounds into a tenure). A
    /// tenure cut short, say a slow leader the round trips moved off at startup, saw only
    /// its first seconds, before its queue grew; one that began idle saw only the writes
    /// since.
    pub(crate) fn spans(&self, sustain: u64) -> bool {
        self.count >= MIN_ROUNDS
            && self.at.0.saturating_sub(self.since.0).saturating_add(2 * PROBE_INTERVAL) >= sustain
    }

    /// Whether this describes its member at `now`: it spans the period and is not stale.
    /// The one rule for the leader's own tenure and for the members it compares against.
    pub(crate) fn measured(&self, now: Timestamp, sustain: u64, stale: u64) -> bool {
        self.spans(sustain) && now.0.saturating_sub(self.at.0) < stale
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(crate) struct Policy {
    tenure: Tenure,
    pending: Pending,
    /// The last round this replica evaluated as leader.
    last_round: Option<Timestamp>,
    /// When the order in force last changed (none: this replica knows of no change).
    changed_at: Option<Timestamp>,
    /// Members miss-based rotation removed from first place, not to be placed first again
    /// until `until`.
    excluded: BTreeMap<NodeId, Exclusion>,
    /// The writers' tail under each member as leader, one sample per round of its latest
    /// tenure. The leader in force's samples are all from its current tenure.
    led: BTreeMap<NodeId, Led>,
}

impl Policy {
    pub(crate) fn new(now: Timestamp, leader: NodeId) -> Self {
        Policy {
            tenure: Tenure { leader, since: now },
            pending: Pending::Idle,
            last_round: None,
            changed_at: None,
            excluded: BTreeMap::new(),
            led: BTreeMap::new(),
        }
    }

    pub(crate) fn target(&self) -> Option<Target> {
        match self.pending {
            Pending::Tracking(t) => Some(t),
            _ => None,
        }
    }

    /// Placement's own order change, from submission until it resolves.
    pub(crate) fn moving(&self) -> Option<ProposalId> {
        match self.pending {
            Pending::Moving(id) => Some(id),
            _ => None,
        }
    }

    /// Whether `member` may not be placed first at `now`.
    pub(crate) fn excluded(&self, member: NodeId, now: Timestamp) -> bool {
        self.excluded.get(&member).is_some_and(|e| e.until > now)
    }

    pub(crate) fn led(&self, member: NodeId) -> Option<Led> {
        self.led.get(&member).copied()
    }

    fn start_tenure(&mut self, leader: NodeId, now: Timestamp) {
        self.tenure = Tenure { leader, since: now };
        // A new tenure is measured afresh.
        self.led.remove(&leader);
    }

    fn untrack(&mut self) {
        if let Pending::Tracking(_) = self.pending {
            self.pending = Pending::Idle;
        }
    }

    fn held(&self, now: Timestamp, hold: u64) -> bool {
        self.changed_at.is_some_and(|t| now.0.saturating_sub(t.0) < hold)
    }

    /// The order in force changed from this slot on and `old` led before it, `new` now. A
    /// decided change takes effect at its epoch's first slot; any other move of the leader
    /// is a rotation, which demotes `old` for a hold, twice as long each time it is demoted
    /// again before it led for a whole hold.
    pub(crate) fn on_order_applied(
        &mut self,
        now: Timestamp,
        hold: u64,
        old: NodeId,
        new: NodeId,
        decided: bool,
    ) {
        self.changed_at = Some(now);
        if new == old {
            return;
        }
        self.start_tenure(new, now);
        if !decided {
            let factor = self.excluded.get(&old).map_or(1, |e| e.factor.saturating_mul(2));
            let until = now.after(hold.saturating_mul(factor));
            self.excluded.insert(old, Exclusion { until, factor });
        }
    }

    /// After a restart or a snapshot: `leader` leads from `now`, as far as this replica saw,
    /// and, if the order ever `changed`, it is treated as just changed.
    pub(crate) fn on_restart(&mut self, now: Timestamp, leader: NodeId, changed: bool) {
        self.start_tenure(leader, now);
        if changed {
            self.changed_at = Some(now);
            self.untrack();
        }
    }

    /// One probe round, leader or not, under `members[0]` as leader: forgets members no
    /// longer in `members`, lifts the leader's exclusion once it led for a whole hold, and
    /// records `us`, the writers' tail this round, once it has led for two whole rounds (the
    /// writers report their previous round).
    pub(crate) fn sample(&mut self, now: Timestamp, hold: u64, members: &[NodeId], us: Option<u64>) {
        let leader = members[0];
        if leader != self.tenure.leader {
            self.start_tenure(leader, now);
        }
        // A leader that led for a whole hold starts over.
        if now.0.saturating_sub(self.tenure.since.0) >= hold {
            self.excluded.remove(&leader);
        }
        self.excluded.retain(|n, _| members.contains(n));
        self.led.retain(|n, _| members.contains(n));
        if now.0.saturating_sub(self.tenure.since.0) < 2 * PROBE_INTERVAL {
            return;
        }
        if let Some(us) = us {
            self.led.entry(leader).or_default().add(us, now);
        }
    }

    /// Placement's own order change `id` was submitted.
    pub(crate) fn on_submitted(&mut self, id: ProposalId) {
        debug_assert!(self.moving().is_none(), "one move at a time");
        self.pending = Pending::Moving(id);
    }

    /// `id` resolved (decided, dropped, fenced or covered by a snapshot); whether it was
    /// placement's own move.
    pub(crate) fn on_resolved(&mut self, id: ProposalId) -> bool {
        if self.pending != Pending::Moving(id) {
            return false;
        }
        self.pending = Pending::Idle;
        true
    }

    /// One evaluation round: the member to put first now, if any. `ranked` is the order in
    /// force and each member's cost (none: unranked), or none if this replica may not move
    /// the lead; `writes` the smoothed writes per round. A move needs, at every round over
    /// the sustained period, the same target to cost at most 80% of the leader in force, and
    /// no order change within a hold; rounds further apart than two probe intervals restart
    /// the count. Nobody gaining by the round trips, the ones they cannot tell apart are
    /// explored.
    pub(crate) fn decide(
        &mut self,
        now: Timestamp,
        periods: Periods,
        writes: u64,
        ranked: Option<(&[NodeId], &[Option<u128>])>,
    ) -> Option<NodeId> {
        let last = self.last_round.replace(now);
        if last.is_none_or(|t| now.0.saturating_sub(t.0) > 2 * PROBE_INTERVAL) {
            self.untrack();
        }
        let Some((order, costs)) = ranked.filter(|_| writes >= WRITE_FLOOR) else {
            self.untrack();
            return None;
        };
        if self.moving().is_some() {
            return None;
        }
        let Some(current) = costs[0] else {
            self.untrack();
            return None;
        };
        let leader = order[0];
        // A gain of a fifth, and more than `TIE_SLACK` a write: round trips of a fraction of
        // a millisecond (one datacenter) differ by a fifth on noise alone.
        let slack = u128::from(writes) * TIE_SLACK;
        let may_lead = |i: usize| {
            costs[i].is_some_and(|c| {
                c.saturating_mul(5) <= current.saturating_mul(4) && current.saturating_sub(c) > slack
            }) && !self.excluded(order[i], now)
        };
        let best = (1..order.len()).filter(|&i| may_lead(i)).min_by_key(|&i| (costs[i], i));
        let tracked = self.target().filter(|t| {
            t.leader == leader && order.iter().position(|&n| n == t.member).is_some_and(may_lead)
        });
        let Some(target) = tracked.or_else(|| Some(Target { member: order[best?], leader, since: now }))
        else {
            self.pending = Pending::Idle;
            return self.explore(now, periods, writes, order, costs);
        };
        self.pending = Pending::Tracking(target);
        if now.0.saturating_sub(target.since.0) < periods.sustain || self.held(now, periods.hold) {
            return None;
        }
        // The gain has held: the member ranked best now goes first (the target kept the
        // period running, but a better member may have become rankable since).
        Some(best.map_or(target.member, |i| order[i]))
    }

    /// Explores the members the round trips cannot tell from the leader, then keeps the
    /// fastest: once its tenure is measured, hands the lead to a tied member not measured
    /// recently, or, every tied member measured, to one the leader is clearly slower than.
    fn explore(
        &self,
        now: Timestamp,
        periods: Periods,
        writes: u64,
        order: &[NodeId],
        costs: &[Option<u128>],
    ) -> Option<NodeId> {
        let Periods { sustain, hold } = periods;
        let stale = hold.saturating_mul(STALE_HOLDS);
        let leader = order[0];
        let current = costs[0]?;
        if self.tenure.leader != leader || now.0.saturating_sub(self.tenure.since.0) < sustain {
            return None;
        }
        let mine = self.led(leader).filter(|l| l.measured(now, sustain, stale))?;
        let writes = u128::from(writes);
        let alike = |c: u128| {
            c.saturating_mul(10) <= current.saturating_mul(TIE).saturating_add(writes * TIE_SLACK * 10)
        };
        // A member demoted by rotation within four holds is not tried: the demotion is a
        // measurement too, and its tenure's samples from before whatever slowed it no longer
        // describe it.
        let demoted =
            |n: NodeId| self.excluded.get(&n).is_some_and(|e| e.until.after(hold.saturating_mul(4)) > now);
        let measured = |n: NodeId| self.led(n).filter(|l| l.measured(now, sustain, stale));
        let tied: Vec<usize> =
            (1..order.len()).filter(|&i| costs[i].is_some_and(alike) && !demoted(order[i])).collect();
        // Exploring: the next tied member not measured, best placed first; no hold between
        // steps, each tenure already took a sustained period.
        if let Some(&i) =
            tied.iter().filter(|&&i| measured(order[i]).is_none()).min_by_key(|&&i| (costs[i], i))
        {
            return Some(order[i]);
        }
        // Exploited: the fastest measured member, if this leader is clearly slower, by more
        // than three standard errors and by more than `TIE`: tested every round, a test alone
        // would sooner or later pass between members that are alike.
        let (i, l) = tied
            .iter()
            .filter_map(|&i| measured(order[i]).map(|l| (i, l)))
            .min_by_key(|(i, l)| (l.mean(), *i))?;
        let faster = mine.worse_than(&l) && l.mean().saturating_mul(TIE) < mine.mean().saturating_mul(10);
        (!self.held(now, hold) && faster).then_some(order[i])
    }
}

/// Reads for tests, and fixtures for those that start from a state no short run of rounds
/// reaches.
#[cfg(test)]
impl Policy {
    pub(crate) fn tenure(&self) -> Tenure {
        self.tenure
    }

    pub(crate) fn pending(&self) -> Pending {
        self.pending
    }

    pub(crate) fn changed_at(&self) -> Option<Timestamp> {
        self.changed_at
    }

    pub(crate) fn exclusion(&self, member: NodeId) -> Option<Exclusion> {
        self.excluded.get(&member).copied()
    }

    pub(crate) fn insert_led(&mut self, member: NodeId, led: Led) {
        self.led.insert(member, led);
    }

    pub(crate) fn insert_exclusion(&mut self, member: NodeId, exclusion: Exclusion) {
        self.excluded.insert(member, exclusion);
    }
}

#[cfg(test)]
#[path = "placement_policy_tests.rs"]
mod tests;

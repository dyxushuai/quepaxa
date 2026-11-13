//! Leader placement, decision side: the leader in force ranks the members by the commit
//! latency each would give the writers, and moves the lead by an ordinary leader-order
//! change when another member keeps a clear gain. This file gathers the rates, round trips
//! and costs; `placement_policy.rs` holds the state and makes the decisions below.
//!
//! The cost of candidate c, from write rates w (per origin) and round trips:
//! `Σ_o w_o · rtt(o, c) + (Σ_o w_o) · quorum(c)`, where `quorum(c)` is the round trip from
//! c to the member that completes a majority of the epoch in force, c itself counted at
//! zero. A candidate is not ranked when a writer's round trip to it, its own table, or
//! enough round trips for a majority are unknown; when the leader in force has no cost,
//! nothing moves. A member that miss-based rotation demoted is not placed first for a hold,
//! twice as long each time it is demoted again before it led for a whole hold, so placement
//! and rotation cannot hand the lead back and forth.
//!
//! A move needs, at every round over `Config::placement_sustain`, the same target to cost at
//! most 80% of the leader in force; no order change within `Config::placement_hold`; at
//! least `WRITE_FLOOR` writes; and nothing pending that changes the order. Rounds further
//! apart than two probe intervals restart the count, so bursts separated by silence never
//! add up. The move is an internal change (as the retirement barrier is): the application
//! never sees its id, and a local `propose_reconfiguration` meanwhile gets
//! `ReconfigurationPending`. Only who leads changes; safety does not depend on any of it.
//!
//! Round trips cannot see a leader that is slow for another reason (a CPU or a disk), since
//! probes are answered off the commit path, and between members they rank alike they cannot
//! choose. So, as the paper's leader tuning does (§5.3), the cluster also measures leaders:
//! every member records, each round, the writers' tail commit latency (each writer's 90th
//! percentile over its last round, from its probes, weighted by its writes) under the
//! leader in force. Unlike the paper, only members the round trips cannot tell apart (within
//! `TIE`, plus `TIE_SLACK` a write) are explored, so across regions, where the round trips
//! already know the answer, exploring costs nothing; and it runs once rather than every few
//! epochs: a leader whose tenure is measured (`MIN_ROUNDS` over a sustained period) hands
//! the lead to the next tied member with no measurement younger than `STALE_HOLDS` holds,
//! back to back, and once every tied member is measured the lead goes to the fastest, if
//! the leader is slower than it by more than three standard errors. A leader that slows
//! down later is moved the same way; a measurement that goes stale is taken again.

use alloc::vec::Vec;

use super::Replica;
use super::placement::fresh;
use crate::batch::{self, Kind};
use crate::placement_policy::Periods;
use crate::types::{NodeId, Slot};

impl Replica {
    /// Folds the round's writes into the rates. Runs at every probe round, leader or not, so
    /// a new leader starts informed.
    pub(super) fn close_round(&mut self) {
        let members = self.applied_schedule.members(Slot(self.emitted)).to_vec();
        let p = &mut self.placement;
        let writes = core::mem::take(&mut p.writes);
        for origin in writes.keys() {
            p.rates.entry(*origin).or_default();
        }
        // A writer converges to eight times its writes per round; one that stopped decays
        // (rounded up) to nothing, and one that left no longer counts.
        p.rates.retain(|origin, rate| {
            *rate = match writes.get(origin).copied().unwrap_or(0) {
                0 => *rate - rate.div_ceil(8),
                n => *rate - *rate / 8 + n,
            };
            *rate > 0 && members.contains(origin)
        });
        let us = self.writers_tail(&members);
        let (now, hold) = (self.now, self.cfg.placement_hold);
        self.placement.policy.sample(now, hold, &members, us);
    }

    /// The round's writer-weighted tail commit latency, in microseconds, if writers with at
    /// least half the writes reported one.
    fn writers_tail(&self, members: &[NodeId]) -> Option<u64> {
        let now = self.now;
        let (mut seen, mut weight, mut total) = (0u128, 0u128, 0u128);
        for (&origin, &rate) in self.placement.rates.iter().filter(|(o, _)| members.contains(o)) {
            let rate = u128::from(rate);
            total += rate;
            let tail = if origin == self.me {
                self.placement.tail
            } else {
                self.placement.observed.get(&origin).filter(|(_, at)| fresh(*at, now)).map(|(t, _)| *t)
            };
            if let Some(tail) = tail {
                seen += rate * u128::from(tail);
                weight += rate;
            }
        }
        if weight == 0 || 2 * weight < total {
            return None;
        }
        Some(u64::try_from(seen / weight / 1_000).unwrap_or(u64::MAX))
    }

    /// The round trip from `c` to the member completing a majority of `members` (`c`
    /// itself at zero), if enough round trips are known.
    fn quorum_rtt(&self, c: NodeId, members: &[NodeId]) -> Option<u64> {
        let mut rtts: Vec<u64> = members.iter().filter_map(|&m| self.rtt_between(c, m)).collect();
        let majority = members.len() / 2 + 1;
        if rtts.len() < majority {
            return None;
        }
        rtts.sort_unstable();
        Some(rtts[majority - 1])
    }

    /// After slot `applied` is applied: notes whether the order changes from the next slot
    /// on, which leader that demotes, and whether a decided change (not a rotation) did it:
    /// a decided change takes effect at its epoch's first slot.
    pub(super) fn note_applied(&mut self, applied: Slot) {
        let next = Slot(applied.0 + 1);
        let (old, new) = (self.applied_schedule.leader(applied), self.applied_schedule.leader(next));
        if self.applied_schedule.members(applied) == self.applied_schedule.members(next) {
            return;
        }
        let decided = self.applied_schedule.epoch().1 == next.0;
        let (now, hold) = (self.now, self.cfg.placement_hold);
        self.placement.policy.on_order_applied(now, hold, old, new, decided);
    }

    /// After a restart or a snapshot, whatever changed before is treated as just changed.
    pub(super) fn restart_hold(&mut self) {
        let leader = self.applied_schedule.leader(Slot(self.emitted));
        let changed = self.applied_schedule.generation() > 0;
        self.placement.policy.on_restart(self.now, leader, changed);
    }

    /// Whether this replica may move the lead now: it leads the order in force and may
    /// change it.
    fn may_move(&self) -> bool {
        self.leads_now() && self.may_change()
    }

    /// Whether this replica may propose an order change now: it has not stepped aside, and
    /// no order change is pending or on its way.
    fn may_change(&self) -> bool {
        let pending_change = |kind: Kind| kind == Kind::Reconfigure;
        self.cfg.placement
            && !self.watch.aside
            && self.schedule.settled(Slot(self.committed))
            && self.placement.policy.moving().is_none()
            && !self.outstanding.values().any(|(_, kind, _)| pending_change(*kind))
            && !self.pending.iter().chain(&self.relayed).any(|i| pending_change(i.kind))
            && !self.batches.values().any(|b| !batch::changes(&b.value).is_empty())
    }

    /// The round trip between `a` and `b` as `a` (or, failing that, `b`) last reported it,
    /// if fresh.
    fn rtt_between(&self, a: NodeId, b: NodeId) -> Option<u64> {
        let row = |from: NodeId, to: NodeId| {
            if from == self.me {
                return self.fresh_rtt(to);
            }
            let (table, at) = self.placement.tables.get(&from)?;
            fresh(*at, self.now).then(|| table.iter().find(|(n, _)| *n == to).map(|(_, r)| *r))?
        };
        if a == b { Some(0) } else { row(a, b).or_else(|| row(b, a)) }
    }

    /// The order for a membership change to `members` from `in_force` with no order given:
    /// staying members by their cost under the members in force (an excluded one after the
    /// rest, ties and unranked ones in their order), then new members by id.
    pub(super) fn placed_order(&self, in_force: &[NodeId], members: &[NodeId]) -> Vec<NodeId> {
        let now = self.now;
        let mut staying: Vec<(usize, NodeId)> =
            in_force.iter().copied().enumerate().filter(|(_, n)| members.contains(n)).collect();
        staying.sort_by_key(|&(i, n)| {
            let excluded = self.placement.policy.excluded(n, now);
            let cost = self.cost(n, in_force);
            (excluded, cost.is_none(), cost, i)
        });
        let mut order: Vec<NodeId> = staying.into_iter().map(|(_, n)| n).collect();
        order.extend(members.iter().copied().filter(|n| !in_force.contains(n)));
        order
    }

    /// Candidate `c`'s cost over `members`, or none if it cannot be ranked.
    fn cost(&self, c: NodeId, members: &[NodeId]) -> Option<u128> {
        let own_table = self.placement.tables.get(&c).is_some_and(|(_, at)| fresh(*at, self.now));
        if c != self.me && !own_table {
            return None;
        }
        let (mut sum, mut total) = (0u128, 0u128);
        for (&origin, &rate) in self.placement.rates.iter().filter(|(o, _)| members.contains(o)) {
            let rtt = self.rtt_between(origin, c)?;
            sum = sum.checked_add(u128::from(rate).checked_mul(u128::from(rtt))?)?;
            total += u128::from(rate);
        }
        sum.checked_add(total.checked_mul(u128::from(self.quorum_rtt(c, members)?))?)
    }

    /// One evaluation round: submits the move the policy decides on, if any.
    pub(super) fn evaluate_placement(&mut self) {
        let now = self.now;
        let periods = Periods { sustain: self.cfg.placement_sustain, hold: self.cfg.placement_hold };
        let writes: u64 = self.placement.rates.values().sum();
        let order = self.schedule.members(Slot(self.committed)).to_vec();
        let costs: Vec<Option<u128>> =
            if self.may_move() { order.iter().map(|&c| self.cost(c, &order)).collect() } else { Vec::new() };
        let ranked = (!costs.is_empty()).then_some((&order[..], &costs[..]));
        if let Some(first) = self.placement.policy.decide(now, periods, writes, ranked) {
            self.move_lead(first, &order, &costs);
        }
    }

    /// Proposes `first` as leader, then the others by cost, unranked ones last in their
    /// order.
    fn move_lead(&mut self, first: NodeId, order: &[NodeId], costs: &[Option<u128>]) {
        let mut rest: Vec<usize> = (0..order.len()).filter(|&i| order[i] != first).collect();
        rest.sort_by_key(|&i| (costs[i].is_none(), costs[i], i));
        let new_order: Vec<NodeId> =
            core::iter::once(first).chain(rest.into_iter().map(|i| order[i])).collect();
        let change = crate::schedule::encode_change(self.schedule.generation(), &new_order);
        // Busy: tried again next round.
        if let Ok(id) = self.submit(change, Kind::Reconfigure) {
            self.metrics.placement_moves += 1;
            self.placement.policy.on_submitted(id);
        }
    }
}

//! Leader placement, decision side: the leader in force ranks the members by the commit
//! latency each would give the writers, and moves the lead by an ordinary leader-order
//! change when another member keeps a clear gain.
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
use super::placement::{Exclusion, PROBE_INTERVAL, Target, fresh};
use crate::batch::{self, Kind};
use crate::types::{NodeId, Slot};

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
pub(super) const WRITE_FLOOR: u64 = 8;

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
        // A leader that led for a whole hold starts over.
        if self.now.0.saturating_sub(p.led_since.0) >= self.cfg.placement_hold {
            p.excluded.remove(&members[0]);
        }
        p.excluded.retain(|n, _| members.contains(n));
        p.led.retain(|n, _| members.contains(n));
        self.sample_leader(&members);
    }

    /// Records the round's writer-weighted tail commit latency under the leader in force,
    /// once it has led for two whole rounds (the writers report their previous round).
    fn sample_leader(&mut self, members: &[NodeId]) {
        let (now, leader) = (self.now, members[0]);
        if now.0.saturating_sub(self.placement.led_since.0) < 2 * PROBE_INTERVAL {
            return;
        }
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
            return;
        }
        let us = u64::try_from(seen / weight / 1_000).unwrap_or(u64::MAX);
        self.placement.led.entry(leader).or_default().add(us, now);
    }

    /// Explores the members the round trips cannot tell from this leader, then keeps the
    /// fastest: once this tenure is measured, hands the lead to a tied member not measured
    /// recently, or, every tied member measured, to one this leader is clearly slower than.
    fn explore(&mut self, order: &[NodeId], costs: &[Option<u128>]) {
        let now = self.now;
        let p = &self.placement;
        let hold = self.cfg.placement_hold;
        let Some(mine) = p.led.get(&self.me).copied() else { return };
        let Some(current) = costs[0] else { return };
        if mine.count < MIN_ROUNDS || now.0.saturating_sub(p.led_since.0) < self.cfg.placement_sustain {
            return;
        }
        let writes: u128 = p.rates.values().map(|&r| u128::from(r)).sum();
        let alike = |c: u128| c.saturating_mul(10) <= current.saturating_mul(TIE) + writes * TIE_SLACK * 10;
        // A member demoted by rotation within four holds is not tried: the demotion is a
        // measurement too, and its tenure's samples from before whatever slowed it no longer
        // describe it.
        let demoted =
            |n: NodeId| p.excluded.get(&n).is_some_and(|e| e.until.after(hold.saturating_mul(4)) > now);
        let measured = |n: NodeId| {
            p.led.get(&n).copied().filter(|l| {
                l.count >= MIN_ROUNDS && now.0.saturating_sub(l.at.0) < hold.saturating_mul(STALE_HOLDS)
            })
        };
        let tied: Vec<usize> =
            (1..order.len()).filter(|&i| costs[i].is_some_and(alike) && !demoted(order[i])).collect();
        // Exploring: the next tied member not measured, best placed first; no hold between
        // steps, each tenure already took a sustained period.
        if let Some(&i) =
            tied.iter().filter(|&&i| measured(order[i]).is_none()).min_by_key(|&&i| (costs[i], i))
        {
            self.move_lead(order[i], order, costs);
            return;
        }
        // Exploited: the fastest measured member, if this leader is clearly slower, by more
        // than three standard errors and by more than `TIE`: tested every round, a test alone
        // would sooner or later pass between members that are alike.
        let held = p.changed_at.is_some_and(|t| now.0.saturating_sub(t.0) < hold);
        let best = tied
            .iter()
            .filter_map(|&i| measured(order[i]).map(|l| (i, l)))
            .min_by_key(|(i, l)| (l.mean(), *i));
        if let Some((i, l)) = best
            && !held
            && mine.worse_than(&l)
            && l.mean().saturating_mul(TIE) < mine.mean().saturating_mul(10)
        {
            self.move_lead(order[i], order, costs);
        }
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
    /// on, which leader that demotes, and whether miss-based rotation (not a decided change)
    /// did it.
    pub(super) fn note_applied(&mut self, applied: Slot) {
        let next = Slot(applied.0 + 1);
        let (old, new) = (self.applied_schedule.leader(applied), self.applied_schedule.leader(next));
        if self.applied_schedule.members(applied) == self.applied_schedule.members(next) {
            return;
        }
        let now = self.now;
        let p = &mut self.placement;
        p.changed_at = Some(now);
        if new == old {
            return;
        }
        p.led_since = now;
        // A new tenure is measured afresh.
        p.led.remove(&new);
        // A decided change takes effect at its epoch's first slot; any other move of the
        // leader is a rotation.
        if self.applied_schedule.epoch().1 != next.0 {
            let factor = p.excluded.get(&old).map_or(1, |e| e.factor.saturating_mul(2));
            let until = now.after(self.cfg.placement_hold.saturating_mul(factor));
            p.excluded.insert(old, Exclusion { until, factor });
        }
    }

    /// After a restart or a snapshot, whatever changed before is treated as just changed.
    pub(super) fn restart_hold(&mut self) {
        // A tenure is measured from when this replica saw it start, not from the clock's epoch.
        self.placement.led_since = self.now;
        if self.applied_schedule.generation() > 0 {
            self.placement.changed_at = Some(self.now);
            self.placement.target = None;
        }
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
            && self.placement.moving.is_none()
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
            let excluded = self.placement.excluded.get(&n).is_some_and(|e| e.until > now);
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

    /// One evaluation round as leader: tracks the target and submits the move when due.
    pub(super) fn evaluate_placement(&mut self) {
        let now = self.now;
        let last = self.placement.last_round.replace(now);
        if last.is_none_or(|t| now.0.saturating_sub(t.0) > 2 * PROBE_INTERVAL) {
            self.placement.target = None;
        }
        let total: u64 = self.placement.rates.values().sum();
        if !self.may_move() || total < WRITE_FLOOR {
            self.placement.target = None;
            return;
        }
        let order = self.schedule.members(Slot(self.committed)).to_vec();
        let leader = order[0];
        let costs: Vec<Option<u128>> = order.iter().map(|&c| self.cost(c, &order)).collect();
        let Some(current) = costs[0] else {
            self.placement.target = None;
            return;
        };
        // A gain of a fifth, and more than `TIE_SLACK` a write: round trips of a fraction of
        // a millisecond (one datacenter) differ by a fifth on noise alone.
        let slack = u128::from(total) * TIE_SLACK;
        let may_lead = |i: usize| {
            costs[i].is_some_and(|c| c.saturating_mul(5) <= current.saturating_mul(4) && current - c > slack)
                && !self.placement.excluded.get(&order[i]).is_some_and(|e| e.until > now)
        };
        let best = (1..order.len()).filter(|&i| may_lead(i)).min_by_key(|&i| (costs[i], i));
        let tracked = self.placement.target.filter(|t| {
            t.leader == leader && order.iter().position(|&n| n == t.member).is_some_and(may_lead)
        });
        let target = tracked.or_else(|| Some(Target { member: order[best?], leader, since: now }));
        self.placement.target = target;
        let Some(target) = target else {
            // Nobody gains by the round trips: measure the ones they cannot tell apart.
            self.explore(&order, &costs);
            return;
        };
        let held =
            self.placement.changed_at.is_some_and(|t| now.0.saturating_sub(t.0) < self.cfg.placement_hold);
        if now.0.saturating_sub(target.since.0) < self.cfg.placement_sustain || held {
            return;
        }
        // The gain has held: the member ranked best now goes first (the target kept the
        // period running, but a better member may have become rankable since), then the
        // others by cost, unranked ones last in their order.
        let first = best.map_or(target.member, |i| order[i]);
        self.move_lead(first, &order, &costs);
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
            self.placement.moving = Some(id);
            self.placement.target = None;
        }
    }
}

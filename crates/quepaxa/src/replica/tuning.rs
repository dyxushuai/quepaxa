//! Leader tuning (the paper's §5.3): the leader in force tries every member as leader, then
//! orders the members by the commit latency writers saw under each.
//!
//! Slots are divided into epochs of `epoch_slots` slots on a grid anchored where the change
//! in force took effect (or where a rotation did). An exploration gives every member, in id
//! order, two epochs as leader (trials `0..2n`), then keeps the last one for an uncounted
//! epoch (trial `2n`) while its reports arrive; its leader then proposes the members sorted
//! by measured latency (`Exploit`). While exploiting, the leader explores again when its
//! latest epoch is significantly slower than the next member's stored latency, or every
//! `Config::tuning_reexplore`. An operator's or a membership change ends any exploration
//! and clears what was measured (`Monitor`).
//!
//! Every member reports, at each epoch's end, the commit latencies of its own entries
//! committed in that epoch by the leader of their slot (`EpochReport`, to every member, so
//! whichever member leads next ranks from the same table). A report names the trial its
//! epoch ran, so a replica that skipped slots by installing a snapshot still knows which
//! epochs belong to the exploration. The phase rides in the decided
//! change (`schedule::Tag`), so every replica and every successive leader agrees on it.
//! Only who leads changes: safety does not depend on any of it.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use super::Replica;
use crate::batch::{self, Kind};
use crate::codec::{Body, LatencyStat};
use crate::schedule::{LAG, Phase, Tag, TuningState};
use crate::types::{NodeId, ProposalId, Slot, Timestamp};

/// Standard errors by which the leader's latest epoch must trail the next member before the
/// cluster explores again: with near-tied members a smaller margin fires every few epochs,
/// since the test runs every epoch.
const SIGNIFICANCE: u128 = 3;
/// Epochs of reports kept beyond those of the exploration in progress.
const KEPT_EPOCHS: usize = 4;

#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct Tuning {
    /// First slot of the epoch being measured and when this replica applied it.
    pub(super) epoch_first: u64,
    pub(super) epoch_started: Timestamp,
    /// Own entries committed in this epoch, by the leader of their slot.
    pub(super) current: Vec<LatencyStat>,
    /// Reports per epoch (first slot), per reporter.
    pub(super) reports: BTreeMap<u64, BTreeMap<NodeId, (bool, Vec<LatencyStat>)>>,
    /// The trial each epoch of the exploration in progress ran (by first slot).
    pub(super) trials: BTreeMap<u64, u16>,
    /// What the last exploration measured per member (pooled over its counted epochs).
    pub(super) rewards: BTreeMap<NodeId, LatencyStat>,
    /// When the period to the next exploration started; none before the first one.
    pub(super) period_from: Option<Timestamp>,
    /// Tuning's own change, from submission until decided, resolved or fenced.
    pub(super) moving: Option<ProposalId>,
    /// Off while the replica replays its log on start: no reports, no proposals.
    pub(super) live: bool,
    /// This replica's hedge multiplier and its trials.
    pub(super) hedge: Hedge,
}

/// Hedge multipliers tried, each for `ARM_EPOCHS` counted epochs of own entries.
pub(super) const ARMS: [u64; 4] = [1, 2, 4, 8];
/// `DEFAULT_ARM`'s index in `ARMS`.
const DEFAULT_INDEX: usize = 2;
const ARM_EPOCHS: u32 = 2;
/// The multiplier kept unless another is significantly faster (today's fixed one).
const DEFAULT_ARM: u64 = 4;

#[derive(Clone, Debug)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct Hedge {
    /// The multiplier in use.
    pub(super) multiplier: u64,
    /// The arm being tried (index into `ARMS`) and its counted epochs so far; none when
    /// settled.
    pub(super) trying: Option<(usize, u32)>,
    /// Own entries per arm in the trials so far.
    pub(super) stats: Vec<LatencyStat>,
    /// Trials are due once leader tuning next exploits.
    pub(super) due: bool,
}

impl Default for Hedge {
    fn default() -> Self {
        Self { multiplier: DEFAULT_ARM, trying: None, stats: Vec::new(), due: true }
    }
}

fn mean(s: &LatencyStat) -> u128 {
    u128::from(s.sum) / u128::from(s.count.max(1))
}

/// Variance of the mean (variance over count), from count, sum and sum of squares.
fn var_of_mean(s: &LatencyStat) -> u128 {
    let n = u128::from(s.count.max(1));
    let sum = u128::from(s.sum);
    let var = s.sum_sq.saturating_sub(sum * sum / n) / n;
    var / n
}

/// The multiplier to keep: the default unless another arm's mean is lower by more than
/// `SIGNIFICANCE` standard errors (the fastest such arm).
pub(super) fn best_arm(stats: &[LatencyStat]) -> u64 {
    let base = &stats[DEFAULT_INDEX];
    if base.count == 0 {
        return DEFAULT_ARM;
    }
    let faster = |s: &LatencyStat| {
        let (a, b) = (mean(s), mean(base));
        let se2 = var_of_mean(s) + var_of_mean(base);
        s.count > 0 && a < b && (b - a) * (b - a) > SIGNIFICANCE * SIGNIFICANCE * se2
    };
    (0..ARMS.len())
        .filter(|&i| faster(&stats[i]))
        .min_by_key(|&i| mean(&stats[i]))
        .map_or(DEFAULT_ARM, |i| ARMS[i])
}

impl Replica {
    fn tuning_state(&self) -> &TuningState {
        &self.applied_tuning
    }

    /// The epoch length in force.
    fn epoch_slots(&self) -> u64 {
        u64::from(self.tuning_state().tag.epoch_slots)
    }

    /// Records the commit latency of an own entry committed in `slot`, if it was proposed
    /// after the epoch began (an older one carries the previous leader's handover).
    pub(super) fn note_commit(&mut self, slot: Slot, proposed: Timestamp) {
        if !self.cfg.tuning || proposed < self.tuning.epoch_started {
            return;
        }
        let leader = self.applied_schedule.leader(slot);
        let us = self.now.0.saturating_sub(proposed.0) / 1_000;
        let current = &mut self.tuning.current;
        match current.iter_mut().find(|s| s.leader == leader) {
            Some(s) => s.add(us),
            None => {
                let mut s = LatencyStat::new(leader);
                s.add(us);
                current.push(s);
            }
        }
    }

    /// After slot `applied` is applied: starts the next epoch where the grid, a change or a
    /// rotation says, then proposes what the phase calls for when this replica is due to.
    pub(super) fn tuning_applied(&mut self, applied: Slot) {
        let next = applied.0 + 1;
        let taking_effect = self.applied_tuning.pending.iter().position(|(from, _)| *from == next);
        let rotated = self.applied_schedule.members(applied) != self.applied_schedule.members(Slot(next));
        let mut by_grid = false;
        if let Some(i) = taking_effect {
            let (_, tag) = self.applied_tuning.pending.remove(i);
            self.applied_tuning.tag = tag;
            self.applied_tuning.anchor = next;
            self.start_epoch(next);
            self.enter_phase(tag.phase, next);
        } else if rotated {
            // A rotation keeps the phase and restarts the grid; the member rotated out gets
            // no counted epoch for its trial.
            self.applied_tuning.anchor = next;
            self.start_epoch(next);
        } else if self.applied_tuning.pending.is_empty()
            && next > self.applied_tuning.anchor
            && (next - self.applied_tuning.anchor).is_multiple_of(self.epoch_slots())
        {
            by_grid = true;
            self.start_epoch(next);
        }
        if !self.cfg.tuning || !self.tuning.live {
            return;
        }
        if next == self.tuning.epoch_first + self.epoch_slots() - LAG && self.leads_now() {
            self.decide_tuning();
        } else if by_grid
            && matches!(self.applied_tuning.tag.phase, Phase::Explore(_))
            && self.tuning_deputy() == Some(self.me)
        {
            // An exploring epoch ended with no change on its way: its leader runs without
            // tuning. The next member that reports proposes in its place; the base
            // generation rejects a second copy.
            self.decide_tuning();
        }
    }

    /// Ends the epoch being measured (reporting it) and starts one at `first`.
    fn start_epoch(&mut self, first: u64) {
        if self.cfg.tuning && self.tuning.live && first > self.tuning.epoch_first {
            let stats = core::mem::take(&mut self.tuning.current);
            self.hedge_epoch_ended(&stats);
            let epoch = Slot(self.tuning.epoch_first);
            let trying_arm = self.trying_arm();
            let trial = self.tuning.trials.get(&epoch.0).copied();
            let me = self.me;
            let members: Vec<NodeId> = self.applied_schedule.members(Slot(first)).to_vec();
            for &to in members.iter().filter(|&&m| m != me) {
                self.send(to, Body::EpochReport { first: epoch, trial, trying_arm, stats: stats.clone() });
            }
            self.store_report(self.me, epoch.0, trial, trying_arm, stats);
        }
        self.tuning.current.clear();
        self.tuning.epoch_first = first;
        self.tuning.epoch_started = self.now;
        if let Phase::Explore(t) = self.applied_tuning.tag.phase {
            self.tuning.trials.insert(first, t);
        }
        self.prune_reports();
    }

    /// Side effects of a phase taking effect at `first`.
    pub(super) fn enter_phase(&mut self, phase: Phase, first: u64) {
        match phase {
            Phase::Monitor => {
                self.tuning.trials.clear();
                self.tuning.rewards.clear();
                self.tuning.period_from = Some(self.now);
            }
            Phase::Explore(0) => {
                self.tuning.trials.clear();
                self.tuning.trials.insert(first, 0);
                self.tuning.rewards.clear();
                self.tuning.period_from = Some(self.now);
                // The arms are tried again once the new order is exploited; meanwhile the
                // multiplier stays.
                let hedge = &mut self.tuning.hedge;
                (hedge.trying, hedge.due) = (None, true);
                hedge.stats.clear();
            }
            Phase::Explore(_) => {}
            Phase::Exploit => {
                self.tuning.rewards = self.measured();
                self.tuning.trials.clear();
            }
        }
    }

    pub(super) fn on_epoch_report(
        &mut self,
        from: NodeId,
        first: Slot,
        trial: Option<u16>,
        trying_arm: bool,
        stats: Vec<LatencyStat>,
    ) {
        if !self.cfg.tuning || !self.applied_schedule.members(Slot(self.emitted)).contains(&from) {
            return;
        }
        self.store_report(from, first.0, trial, trying_arm, stats);
    }

    pub(super) fn store_report(
        &mut self,
        from: NodeId,
        first: u64,
        trial: Option<u16>,
        trying_arm: bool,
        mut stats: Vec<LatencyStat>,
    ) {
        let members = self.applied_schedule.members(Slot(self.emitted));
        stats.retain(|s| members.contains(&s.leader));
        // Trials run 0..=2n; anything else is not one of this membership's.
        if let Some(t) = trial.filter(|t| usize::from(*t) <= 2 * members.len()) {
            self.tuning.trials.entry(first).or_insert(t);
            // Only the latest exploration counts: it starts at its last trial-0 epoch.
            let start = self.tuning.trials.iter().filter(|(_, t)| **t == 0).map(|(f, _)| *f).max();
            if let Some(start) = start {
                self.tuning.trials = self.tuning.trials.split_off(&start);
            }
        }
        self.tuning.reports.entry(first).or_default().insert(from, (trying_arm, stats));
        self.prune_reports();
    }

    fn prune_reports(&mut self) {
        let oldest_trial = self.tuning.trials.keys().next().copied();
        let keep_from = self.tuning.reports.keys().rev().nth(KEPT_EPOCHS).copied();
        let cut = match (oldest_trial, keep_from) {
            (Some(t), Some(k)) => t.min(k),
            (_, k) => k.unwrap_or(0),
        };
        self.tuning.reports = self.tuning.reports.split_off(&cut);
    }

    /// Every reporter's entries in epoch `first` under `leader`, and whether any reporter was
    /// trying a hedge multiplier then.
    pub(super) fn epoch_stat(&self, first: u64, leader: NodeId) -> (LatencyStat, bool) {
        let mut total = LatencyStat::new(leader);
        let mut arm = false;
        for (trying, stats) in self.tuning.reports.get(&first).into_iter().flat_map(BTreeMap::values) {
            arm |= *trying;
            for s in stats.iter().filter(|s| s.leader == leader) {
                total.merge(s);
            }
        }
        (total, arm)
    }

    /// Whether `stat` has enough entries to count.
    fn counts(&self, stat: &LatencyStat) -> bool {
        stat.count >= u64::from(self.cfg.tuning_min_samples.max(1))
    }

    /// The members in id order: the exploration's sequence.
    fn roster(&self) -> Vec<NodeId> {
        let mut roster = self.schedule.members(Slot(self.committed)).to_vec();
        roster.sort_unstable();
        roster
    }

    /// What the exploration in progress measured: each trial member's counted epochs, pooled.
    pub(super) fn measured(&self) -> BTreeMap<NodeId, LatencyStat> {
        let roster = self.roster();
        let mut out: BTreeMap<NodeId, LatencyStat> = BTreeMap::new();
        for (&first, &t) in &self.tuning.trials {
            let Some(&member) = roster.get(usize::from(t) / 2).filter(|_| usize::from(t) < 2 * roster.len())
            else {
                continue;
            };
            let (stat, _) = self.epoch_stat(first, member);
            if self.counts(&stat) {
                out.entry(member).or_insert(LatencyStat::new(member)).merge(&stat);
            }
        }
        out
    }

    /// Whether this replica may change the order now: it is due to (as leader or deputy),
    /// has not stepped aside, and no order change is pending or on its way.
    fn may_tune(&self) -> bool {
        let pending_change = |kind: Kind| kind == Kind::Reconfigure;
        !self.watch.aside
            && self.schedule.settled(Slot(self.committed))
            && self.tuning.moving.is_none()
            && !self.outstanding.values().any(|(_, kind, _)| pending_change(*kind))
            && !self.pending.iter().chain(&self.relayed).any(|i| pending_change(i.kind))
            && !self.batches.values().any(|b| !batch::changes(&b.value).is_empty())
    }

    /// The member after the leader that proposes in its place: the first one in the order
    /// that reported the last epoch (this replica counts as reporting).
    pub(super) fn tuning_deputy(&self) -> Option<NodeId> {
        let order = self.schedule.members(Slot(self.committed));
        let last = self.tuning.reports.range(..self.tuning.epoch_first).next_back().map(|(_, r)| r);
        order.iter().skip(1).copied().find(|&m| m == self.me || last.is_some_and(|r| r.contains_key(&m)))
    }

    /// Proposes the change the phase in force calls for, if any.
    pub(super) fn decide_tuning(&mut self) {
        let order = self.schedule.members(Slot(self.committed)).to_vec();
        // A single member has no one to compare with.
        if order.len() < 2 || !self.may_tune() {
            return;
        }
        let roster = self.roster();
        let n = roster.len();
        let trial_order = |first: NodeId| -> Vec<NodeId> {
            core::iter::once(first).chain(order.iter().copied().filter(|&m| m != first)).collect()
        };
        let explore = (trial_order(roster[0]), Phase::Explore(0));
        let proposal = match self.applied_tuning.tag.phase {
            Phase::Explore(t) => {
                let t = usize::from(t);
                if t + 1 < 2 * n {
                    // Trial t is led by the member at t / 2 in id order.
                    let trial = t + 1;
                    Some((trial_order(roster[trial / 2]), Phase::Explore(trial as u16)))
                } else if t + 1 == 2 * n {
                    Some((order.clone(), Phase::Explore((t + 1) as u16)))
                } else if (0..2 * n).all(|t| self.tuning.trials.values().any(|&u| usize::from(u) == t)) {
                    Some((self.sorted_order(&order), Phase::Exploit))
                } else {
                    // This replica missed part of the exploration (it restarted or installed
                    // a snapshot since): measure again rather than sort on a partial table.
                    Some(explore)
                }
            }
            Phase::Exploit | Phase::Monitor => {
                let due = self.tuning.period_from.is_none_or(|from| {
                    self.cfg.tuning_reexplore > 0
                        && self.now.0.saturating_sub(from.0) >= self.cfg.tuning_reexplore
                });
                (due || self.behind_next(&order)).then_some(explore)
            }
        };
        let Some((new_order, phase)) = proposal else { return };
        let tag = Tag { phase, epoch_slots: self.cfg.tuning_epoch_slots };
        let change = crate::schedule::encode_change(self.schedule.generation(), &new_order, tag);
        // Busy: tried again at the next epoch.
        if let Ok(id) = self.submit(change, Kind::Reconfigure) {
            self.metrics.tuning_moves += 1;
            self.tuning.moving = Some(id);
        }
    }

    /// The members sorted by measured latency, best first; unmeasured ones last in `order`.
    fn sorted_order(&self, order: &[NodeId]) -> Vec<NodeId> {
        let measured = self.measured();
        let mut ranked: Vec<NodeId> = measured.keys().copied().collect();
        ranked.sort_by_key(|m| (mean(&measured[m]), *m));
        ranked.extend(order.iter().copied().filter(|m| !measured.contains_key(m)));
        ranked
    }

    /// Whether the leader's latest finished epoch trails the next member's stored latency by
    /// more than `SIGNIFICANCE` standard errors (epochs in which anyone tried a hedge
    /// multiplier do not count).
    fn behind_next(&self, order: &[NodeId]) -> bool {
        let (Some(&leader), Some(next)) = (order.first(), order.get(1)) else { return false };
        let Some(stored) = self.tuning.rewards.get(next) else { return false };
        let Some((&first, _)) = self.tuning.reports.range(..self.tuning.epoch_first).next_back() else {
            return false;
        };
        let (latest, arm) = self.epoch_stat(first, leader);
        if arm || !self.counts(&latest) {
            return false;
        }
        let (a, b) = (mean(&latest), mean(stored));
        let se2 = var_of_mean(&latest) + var_of_mean(stored);
        a > b && (a - b) * (a - b) > SIGNIFICANCE * SIGNIFICANCE * se2
    }

    /// What the last exploration measured for `member`, in nanoseconds.
    pub(super) fn led_latency(&self, member: NodeId) -> Option<u64> {
        self.tuning.rewards.get(&member).map(|s| (mean(s) as u64).saturating_mul(1_000))
    }

    /// The tuning phase as a gauge: 0 monitoring (or off), 1 exploring, 2 exploiting.
    pub(super) fn tuning_phase(&self) -> u64 {
        match (self.cfg.tuning, self.applied_tuning.tag.phase) {
            (false, _) | (true, Phase::Monitor) => 0,
            (true, Phase::Explore(_)) => 1,
            (true, Phase::Exploit) => 2,
        }
    }

    /// Whether this replica is trying a hedge multiplier now.
    fn trying_arm(&self) -> bool {
        self.tuning.hedge.trying.is_some()
    }

    /// This replica's hedge multiplier.
    pub(super) fn hedge_multiplier(&self) -> u64 {
        if self.cfg.tuning && self.cfg.hedge_tuning { self.tuning.hedge.multiplier } else { DEFAULT_ARM }
    }

    /// At an epoch's end: counts its own entries toward the arm being tried, moves to the
    /// next arm, or settles. Trials run only while leader tuning exploits, so the leader
    /// stays the same across them.
    pub(super) fn hedge_epoch_ended(&mut self, epoch: &[LatencyStat]) {
        if !self.cfg.hedge_tuning {
            return;
        }
        let exploiting = self.applied_tuning.tag.phase == Phase::Exploit;
        let min = u64::from(self.cfg.tuning_min_samples.max(1));
        let hedge = &mut self.tuning.hedge;
        match hedge.trying {
            None if hedge.due && exploiting => {
                hedge.due = false;
                hedge.stats = ARMS.iter().map(|_| LatencyStat::new(self.me)).collect();
                hedge.trying = Some((0, 0));
                hedge.multiplier = ARMS[0];
            }
            None => {}
            Some(_) if !exploiting => {
                // The order changed under the trials: start them over at the next exploit.
                (hedge.trying, hedge.due, hedge.multiplier) = (None, true, DEFAULT_ARM);
                hedge.stats.clear();
            }
            Some((arm, counted)) => {
                let mut total = LatencyStat::new(self.me);
                for s in epoch {
                    total.merge(s);
                }
                let counted = if total.count >= min {
                    hedge.stats[arm].merge(&total);
                    counted + 1
                } else {
                    counted
                };
                if counted < ARM_EPOCHS {
                    hedge.trying = Some((arm, counted));
                } else if arm + 1 < ARMS.len() {
                    hedge.trying = Some((arm + 1, 0));
                    hedge.multiplier = ARMS[arm + 1];
                } else {
                    hedge.trying = None;
                    hedge.multiplier = best_arm(&hedge.stats);
                }
            }
        }
    }

    /// After installing a snapshot: measuring continues on the installed grid and phase,
    /// keeping the reports received meanwhile (they name their trials), so a replica that
    /// skipped slots still ranks from the exploration's whole table.
    pub(super) fn resync_tuning(&mut self) {
        let (anchor, e) = (self.applied_tuning.anchor, self.epoch_slots());
        let first = anchor + self.emitted.saturating_sub(anchor) / e * e;
        self.tuning.current.clear();
        self.tuning.epoch_first = first;
        self.tuning.epoch_started = self.now;
        self.tuning.period_from.get_or_insert(self.now);
        match self.applied_tuning.tag.phase {
            Phase::Explore(t) => {
                self.tuning.trials.entry(first).or_insert(t);
            }
            // The exploration ended in the slots skipped.
            Phase::Exploit if !self.tuning.trials.is_empty() => {
                self.tuning.rewards = self.measured();
                self.tuning.trials.clear();
            }
            Phase::Exploit => {}
            Phase::Monitor => {
                self.tuning.trials.clear();
                self.tuning.rewards.clear();
            }
        }
    }

    /// Starts measuring from the applied prefix (when the replica starts).
    pub(super) fn restart_tuning(&mut self) {
        let fresh =
            Tuning { live: true, hedge: core::mem::take(&mut self.tuning.hedge), ..Tuning::default() };
        let never_changed = self.applied_tuning == TuningState::initial(self.applied_tuning.tag.epoch_slots);
        self.tuning = Tuning { period_from: (!never_changed).then_some(self.now), ..fresh };
        let (anchor, e) = (self.applied_tuning.anchor, self.epoch_slots());
        let first = anchor + self.emitted.saturating_sub(anchor) / e * e;
        self.tuning.epoch_first = first;
        self.tuning.epoch_started = self.now;
        if let Phase::Explore(t) = self.applied_tuning.tag.phase {
            self.tuning.trials.insert(first, t);
        }
    }
}

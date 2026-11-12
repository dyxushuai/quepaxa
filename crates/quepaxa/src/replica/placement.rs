//! Leader placement, measurement side: round trips between members and where writes come
//! from. `ranking.rs` decides from them where the leader goes.
//!
//! With `Config::placement`, a member probes every other member of the epoch in force once a
//! `PROBE_INTERVAL` while writes are being applied: an `RttProbe` carries its clock and its
//! own smoothed round trips, the receiver echoes the clock in an `RttReply`, and the sender
//! folds the measured round trip into a per-member average. The receiver also keeps the
//! table, so the leader holds every member's view. Writes are counted per origin as they
//! are applied, so no member reports a rate. Members without placement neither probe nor
//! answer: their round trips stay unknown.
//!
//! Probing stops `ACTIVE_FOR` after the last write is applied, so an idle cluster goes
//! quiet, and starts again with the next write. Nothing here is persisted: a restarted
//! replica measures afresh.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use super::Replica;
use crate::batch::{Entry, Item, Kind};
use crate::codec::Body;
use crate::types::{NodeId, ProposalId, Slot, Timestamp};

/// How often a member probes while writes flow.
pub(super) const PROBE_INTERVAL: u64 = 1_000_000_000;
/// A round trip or table older than this is unknown.
pub(super) const FRESH_FOR: u64 = 4 * PROBE_INTERVAL;
/// Own commit latencies kept per probe round for its percentile.
const LATENCY_SAMPLES: usize = 1024;

/// Probing stops this long after the last write is applied.
pub(super) const ACTIVE_FOR: u64 = 4 * PROBE_INTERVAL;

#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct Placement {
    /// Smoothed round trip to each member, in nanoseconds, and when it was last sampled.
    pub(super) rtt: BTreeMap<NodeId, (u64, Timestamp)>,
    /// Send times of the probes each member has not answered yet, oldest first, as many as
    /// fit in `FRESH_FOR` (a round trip longer than `PROBE_INTERVAL` is still measured).
    /// Only a reply to one of them counts, and it retires the older ones too: a duplicate,
    /// a reply to a previous incarnation's probe, or one from a member no longer probed
    /// would skew the average.
    pub(super) awaiting: BTreeMap<NodeId, Vec<u64>>,
    /// Each member's latest round-trip table, from its probes, and when it arrived.
    pub(super) tables: BTreeMap<NodeId, (Vec<(NodeId, u64)>, Timestamp)>,
    /// Commit latencies of this replica's own entries since the last probe round, at most
    /// `LATENCY_SAMPLES` of them.
    pub(super) latencies: Vec<u64>,
    /// Each member's 90th-percentile commit latency of its own entries over its last probe
    /// round, from its probes, and when it arrived.
    pub(super) observed: BTreeMap<NodeId, (u64, Timestamp)>,
    /// The writers' tail commit latency under each member as leader, one sample per round
    /// of its latest tenure (`ranking.rs`, exploration).
    pub(super) led: BTreeMap<NodeId, Led>,
    /// This replica's 90th-percentile commit latency over the last probe round with writes.
    pub(super) tail: Option<u64>,
    /// Application entries applied per origin since the last probe round.
    pub(super) writes: BTreeMap<NodeId, u64>,
    /// Smoothed writes per probe round, per origin, times eight.
    pub(super) rates: BTreeMap<NodeId, u64>,
    /// When an application entry was last applied.
    pub(super) last_write: Option<Timestamp>,
    /// When to probe next; none while idle or disabled.
    pub(super) next_probe: Option<Timestamp>,
    /// The last round this replica evaluated as leader.
    pub(super) last_round: Option<Timestamp>,
    /// The member that has kept a gain over the leader in force, and since when.
    pub(super) target: Option<Target>,
    /// When the order in force last changed (none: this replica knows of no change).
    pub(super) changed_at: Option<Timestamp>,
    /// When the leader in force became leader, as far as this replica saw.
    pub(super) led_since: Timestamp,
    /// Members miss-based rotation removed from first place, not to be placed first again
    /// until `until`.
    pub(super) excluded: BTreeMap<NodeId, Exclusion>,
    /// Placement's own order change, from submission until decided, resolved or fenced.
    pub(super) moving: Option<ProposalId>,
}

/// Per-round samples (microseconds): count, sum, sum of squares, and the latest round.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct Led {
    pub(super) count: u64,
    pub(super) sum: u128,
    pub(super) sum_sq: u128,
    pub(super) at: Timestamp,
}

impl Led {
    pub(super) fn add(&mut self, us: u64, at: Timestamp) {
        self.count += 1;
        self.sum = self.sum.saturating_add(u128::from(us));
        self.sum_sq = self.sum_sq.saturating_add(u128::from(us) * u128::from(us));
        self.at = at;
    }

    pub(super) fn mean(&self) -> u128 {
        self.sum / u128::from(self.count.max(1))
    }

    /// Variance of the mean.
    fn var_of_mean(&self) -> u128 {
        let n = u128::from(self.count.max(1));
        self.sum_sq.saturating_sub(self.sum * self.sum / n) / n / n
    }

    /// Whether these samples are slower than `other`'s by more than three standard errors.
    pub(super) fn worse_than(&self, other: &Led) -> bool {
        let (a, b) = (self.mean(), other.mean());
        a > b && (a - b) * (a - b) > 9 * (self.var_of_mean() + other.var_of_mean())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct Target {
    pub(super) member: NodeId,
    pub(super) leader: NodeId,
    pub(super) since: Timestamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct Exclusion {
    pub(super) until: Timestamp,
    /// Holds this exclusion lasts: doubled each time the member is demoted again before it
    /// led for a whole hold.
    pub(super) factor: u64,
}

pub(super) fn fresh(at: Timestamp, now: Timestamp) -> bool {
    now.0.saturating_sub(at.0) <= FRESH_FOR
}

impl Replica {
    /// Counts applied application entries by origin and keeps probing while they flow.
    pub(super) fn count_writes(&mut self, items: &[Item]) {
        if !self.cfg.placement {
            return;
        }
        let p = &mut self.placement;
        let mut any = false;
        for Item { entry: Entry { origin, .. }, kind } in items {
            if *kind == Kind::App {
                *p.writes.entry(*origin).or_default() += 1;
                any = true;
            }
        }
        if any {
            p.last_write = Some(self.now);
            p.next_probe.get_or_insert(self.now);
        }
    }

    /// Keeps an own entry's commit latency for this round's percentile.
    pub(super) fn note_commit_latency(&mut self, ns: u64) {
        if self.cfg.placement && self.placement.latencies.len() < LATENCY_SAMPLES {
            self.placement.latencies.push(ns);
        }
    }

    /// The smoothed round trip to `member`, if sampled recently.
    pub(super) fn fresh_rtt(&self, member: NodeId) -> Option<u64> {
        self.placement.rtt.get(&member).filter(|(_, at)| fresh(*at, self.now)).map(|(rtt, _)| *rtt)
    }

    /// Probes the members in force if writes were applied recently; otherwise stops.
    pub(super) fn fire_probe(&mut self) {
        let now = self.now;
        if !self.placement.next_probe.is_some_and(|t| t <= now) {
            return;
        }
        let active = self.placement.last_write.is_some_and(|t| now.0.saturating_sub(t.0) <= ACTIVE_FOR);
        if !active {
            self.placement.next_probe = None;
            return;
        }
        self.placement.next_probe = Some(now.after(PROBE_INTERVAL));
        // The tail, not the average: a leader starved of CPU commits most entries quickly
        // and stalls the rest for a scheduler period.
        let mut l = core::mem::take(&mut self.placement.latencies);
        if !l.is_empty() {
            l.sort_unstable();
            self.placement.tail = Some(l[(l.len() - 1) * 9 / 10]);
        }
        self.close_round();
        let members = self.schedule.members(Slot(self.committed)).to_vec();
        // Members of earlier epochs are no longer measured or ranked.
        self.placement.rtt.retain(|n, _| members.contains(n));
        self.placement.tables.retain(|n, _| members.contains(n));
        self.placement.observed.retain(|n, _| members.contains(n));
        self.placement.awaiting.retain(|n, _| members.contains(n));
        if !members.contains(&self.me) {
            return;
        }
        let table: Vec<(NodeId, u64)> =
            members.iter().filter_map(|&m| Some((m, self.fresh_rtt(m)?))).collect();
        let me = self.me;
        for &to in members.iter().filter(|&&m| m != me) {
            let sent = self.placement.awaiting.entry(to).or_default();
            if sent.len() as u64 >= FRESH_FOR / PROBE_INTERVAL {
                sent.remove(0);
            }
            sent.push(now.0);
            let (incarnation, commit) = (self.incarnation, self.placement.tail.unwrap_or(0));
            self.send(to, Body::RttProbe { incarnation, sent: now.0, commit, table: table.clone() });
        }
        self.evaluate_placement();
    }

    pub(super) fn on_rtt_probe(
        &mut self,
        from: NodeId,
        incarnation: u64,
        sent: u64,
        commit: u64,
        mut table: Vec<(NodeId, u64)>,
    ) {
        if !self.cfg.placement {
            return;
        }
        let members = self.schedule.members(Slot(self.committed));
        if !members.contains(&from) {
            return;
        }
        table.retain(|(n, _)| members.contains(n));
        self.placement.tables.insert(from, (table, self.now));
        if commit > 0 {
            self.placement.observed.insert(from, (commit, self.now));
        } else {
            self.placement.observed.remove(&from);
        }
        self.send(from, Body::RttReply { incarnation, sent });
    }

    pub(super) fn on_rtt_reply(&mut self, from: NodeId, incarnation: u64, sent: u64) {
        // A previous incarnation's probe may share a send time with this one's: a host may
        // start its clock over on restart.
        if !self.cfg.placement || incarnation != self.incarnation || sent > self.now.0 {
            return;
        }
        let Some(probes) = self.placement.awaiting.get_mut(&from) else { return };
        let Some(i) = probes.iter().position(|&s| s == sent) else { return };
        probes.drain(..=i);
        let sample = self.now.0 - sent;
        let now = self.now;
        // An average not sampled for a while says nothing about the path now: start over.
        match self.placement.rtt.get_mut(&from) {
            Some((avg, at)) if fresh(*at, now) => {
                *avg = *avg - *avg / 8 + sample / 8;
                *at = now;
            }
            _ => {
                self.placement.rtt.insert(from, (sample, now));
            }
        }
    }
}

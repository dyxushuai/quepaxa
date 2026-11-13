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
pub(super) use crate::placement_policy::PROBE_INTERVAL;
use crate::placement_policy::Policy;
use crate::types::{NodeId, Slot, Timestamp};

/// A round trip or table older than this is unknown.
pub(super) const FRESH_FOR: u64 = 4 * PROBE_INTERVAL;
/// Own commit latencies kept per probe round for its percentile: past this, every other one
/// is dropped and half as many are kept from then on, so the samples span the whole round.
const LATENCY_SAMPLES: usize = 1024;

/// Probing stops this long after the last write is applied.
pub(super) const ACTIVE_FOR: u64 = 4 * PROBE_INTERVAL;

#[derive(Clone, Debug)]
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
    /// Commit latencies of this replica's own entries since the last probe round: one in
    /// `latency_stride` of them, at most `LATENCY_SAMPLES`.
    pub(super) latencies: Vec<u64>,
    /// Own commits this round, and how many of them make one sample.
    pub(super) latency_seen: u64,
    pub(super) latency_stride: u64,
    /// Each member's 90th-percentile commit latency of its own entries over its last probe
    /// round, from its probes, and when it arrived.
    pub(super) observed: BTreeMap<NodeId, (u64, Timestamp)>,
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
    /// The tenure in force, the move in progress, exclusions and measurements.
    pub(super) policy: Policy,
}

impl Placement {
    /// Nothing measured yet; `leader` leads from `now`.
    pub(super) fn new(now: Timestamp, leader: NodeId) -> Self {
        Placement {
            rtt: BTreeMap::new(),
            awaiting: BTreeMap::new(),
            tables: BTreeMap::new(),
            latencies: Vec::new(),
            latency_seen: 0,
            latency_stride: 0,
            observed: BTreeMap::new(),
            tail: None,
            writes: BTreeMap::new(),
            rates: BTreeMap::new(),
            last_write: None,
            next_probe: None,
            policy: Policy::new(now, leader),
        }
    }
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
        if !self.cfg.placement {
            return;
        }
        let p = &mut self.placement;
        let stride = p.latency_stride.max(1);
        p.latency_seen += 1;
        if !(p.latency_seen - 1).is_multiple_of(stride) {
            return;
        }
        p.latencies.push(ns);
        if p.latencies.len() >= LATENCY_SAMPLES {
            let mut i = 0;
            p.latencies.retain(|_| {
                i += 1;
                i % 2 == 1
            });
            p.latency_stride = stride * 2;
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
        (self.placement.latency_seen, self.placement.latency_stride) = (0, 1);
        // A round without own commits measures nothing: an older tail would be reported, and
        // counted under whoever leads now, as if fresh.
        self.placement.tail = (!l.is_empty()).then(|| {
            l.sort_unstable();
            l[(l.len() - 1) * 9 / 10]
        });
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

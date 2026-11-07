//! Leader placement, measurement side: round trips between members and where writes come
//! from.
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
use crate::types::{NodeId, Slot, Timestamp};

/// How often a member probes while writes flow.
pub(super) const PROBE_INTERVAL: u64 = 1_000_000_000;
/// A round trip or table older than this is unknown.
pub(super) const FRESH_FOR: u64 = 4 * PROBE_INTERVAL;
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
    /// Application entries applied, per origin.
    pub(super) writes: BTreeMap<NodeId, u64>,
    /// When an application entry was last applied.
    pub(super) last_write: Option<Timestamp>,
    /// When to probe next; none while idle or disabled.
    pub(super) next_probe: Option<Timestamp>,
}

fn fresh(at: Timestamp, now: Timestamp) -> bool {
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
        let members = self.schedule.members(Slot(self.committed)).to_vec();
        // Members of earlier epochs are no longer measured or ranked.
        self.placement.rtt.retain(|n, _| members.contains(n));
        self.placement.tables.retain(|n, _| members.contains(n));
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
            let incarnation = self.incarnation;
            self.send(to, Body::RttProbe { incarnation, sent: now.0, table: table.clone() });
        }
    }

    pub(super) fn on_rtt_probe(
        &mut self,
        from: NodeId,
        incarnation: u64,
        sent: u64,
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

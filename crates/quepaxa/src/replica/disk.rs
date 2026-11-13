//! This replica's disk as its watch sees it: writing, stuck, or stuck with every other disk.
//!
//! One state, changed only by its events: a write issued (`issued`), writes completed
//! (`completed`), and another replica's decision (`decided_elsewhere`). Its phase (`health`)
//! is read at a moment against the hedge delay δ, which moves with the commit latency, not
//! with the disk: time passing (a timer firing) moves the phase on without an event. The
//! step-aside latch (`Watch::aside`) is a role decision on top of it (`Replica::check_aside`).
//!
//! Proved on a port in `verification/verus/disk_health.rs`.

use alloc::collections::VecDeque;

use crate::types::Timestamp;

/// Where the disk stands at a moment (`Disk::health`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Health {
    /// No write in flight.
    Idle,
    /// Writes in flight, and the disk is not stuck: completing them, perhaps slowly, or not
    /// for long yet. The code draws no line between a healthy disk and a slow one.
    Writing,
    /// Since `since`, no write has completed for δ (at least four usual write times) while
    /// one was in flight, and another replica has decided a slot since the oldest was due:
    /// other disks work.
    Stuck { since: Timestamp },
    /// Stuck, and nobody has decided a slot since the oldest write was due (four usual write
    /// times after it was issued): every disk seems stuck, one shared disk. A decision made
    /// durable just before every disk stopped may still arrive after the write was issued.
    SharedStall { since: Timestamp },
}

#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct Disk {
    /// Writes issued and not yet durable: id and when.
    writes: VecDeque<(u64, Timestamp)>,
    /// Smoothed time from issuing a write to its being durable, in nanoseconds.
    usual: Option<u64>,
    /// When the disk last completed a write.
    wrote: Timestamp,
    /// When another replica last told this one of a slot it decided.
    decided_elsewhere: Timestamp,
    /// When the disk last caught up from a shared stall.
    resumed: Timestamp,
}

impl Disk {
    pub(super) fn issued(&mut self, id: u64, now: Timestamp) {
        self.writes.push_back((id, now));
    }

    /// Writes up to `id` are durable. Caught up from a shared stall, it records when
    /// (`resumed`), which the hedge delay of entries held meanwhile counts from.
    pub(super) fn completed(&mut self, id: u64, now: Timestamp, d: u64) {
        if matches!(self.health(now, d), Health::SharedStall { .. }) {
            self.resumed = now;
        }
        self.wrote = now;
        while let Some(&(p, at)) = self.writes.front()
            && p <= id
        {
            self.writes.pop_front();
            let sample = now.0.saturating_sub(at.0);
            self.usual = Some(self.usual.map_or(sample, |avg| avg - avg / 8 + sample / 8));
        }
    }

    /// Another replica decided a slot this one did not have: a majority made a value durable
    /// without it.
    pub(super) fn decided_elsewhere(&mut self, now: Timestamp) {
        self.decided_elsewhere = now;
    }

    /// The phase at `now` for hedge delay `d`. Stuck counts from the last completed write,
    /// not from when the oldest was issued: with group commit a write on a slow disk waits
    /// for the flush in progress and then its own, so it can wait δ while the disk keeps
    /// completing writes.
    pub(super) fn health(&self, now: Timestamp, d: u64) -> Health {
        let Some(&(_, oldest)) = self.writes.front() else { return Health::Idle };
        let Some(usual) = self.usual else { return Health::Writing };
        let usual4 = usual.saturating_mul(4);
        let since = oldest.max(self.wrote).after(d.max(usual4));
        if since > now {
            Health::Writing
        } else if self.decided_elsewhere <= oldest.after(usual4) {
            Health::SharedStall { since }
        } else {
            Health::Stuck { since }
        }
    }

    pub(super) fn resumed(&self) -> Timestamp {
        self.resumed
    }

    /// When the oldest write in flight will have waited `wait` (at least four usual write
    /// times), if another replica has decided a slot since it was issued: this replica's
    /// slots stall while others' progress.
    pub(super) fn alone_at(&self, wait: u64) -> Option<Timestamp> {
        let usual = self.usual?;
        let &(_, oldest) = self.writes.front()?;
        (self.decided_elsewhere > oldest).then(|| oldest.after(wait.max(usual.saturating_mul(4))))
    }
}

#[cfg(test)]
impl Disk {
    /// Starts from a known usual write time, as after a run of `ns`-long writes.
    pub(super) fn assume_usual(&mut self, ns: u64) {
        self.usual = Some(ns);
    }

    pub(super) fn usual(&self) -> Option<u64> {
        self.usual
    }

    /// Ids of the writes in flight, oldest first.
    pub(super) fn in_flight(&self) -> impl ExactSizeIterator<Item = u64> + DoubleEndedIterator + '_ {
        self.writes.iter().map(|(id, _)| *id)
    }

    pub(super) fn last_decided_elsewhere(&self) -> Timestamp {
        self.decided_elsewhere
    }
}

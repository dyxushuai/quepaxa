//! Retirement: a replica removed by a membership change learns when it may shut down.
//!
//! The slots before a new epoch were decided by the old members. If a majority of them shut
//! down before the new members know those slots, nobody can recover them. So a removed
//! replica waits until a majority of the new members report a durable `commit_index` of at
//! least the epoch's first slot. Catch-up replies carry that: they are sent after every
//! earlier write is durable, and always, even with nothing to return.
//!
//! It may not have heard of a later change adding it back yet, so it then takes a read
//! barrier of its own: once that commits, every change decided before it is applied, and
//! only one decided after the barrier can still come.

use alloc::vec::Vec;

use bytes::Bytes;

use super::Replica;
use crate::api::Action;
use crate::batch::Kind;
use crate::codec::Body;
use crate::types::{NodeId, Slot};

impl Replica {
    /// The latest epoch and its first slot, if this replica is not a member of it and has
    /// not reported it retired.
    fn removed_from(&self) -> Option<(u64, u64)> {
        let (epoch, from) = self.schedule.epoch();
        let removed = epoch > 0 && !self.schedule.members(Slot(from)).contains(&self.me);
        (removed && self.retired != Some(epoch)).then_some((epoch, from))
    }

    /// Notes a member's durable progress, from the envelope of a catch-up reply.
    pub(super) fn note_progress(&mut self, member: NodeId, commit_index: u64) {
        if self.removed_from().is_some() {
            let p = self.member_progress.entry(member).or_insert(0);
            *p = (*p).max(commit_index);
        }
    }

    /// Once a majority of the new members durably know every slot before their epoch,
    /// submits the confirming retirement barrier entry; until then, asks the members still
    /// behind on the retransmit timer.
    pub(super) fn check_retired(&mut self) {
        if let Some(epoch) = self.retire_committed.take()
            && self.removed_from().is_some_and(|(latest, _)| latest == epoch)
        {
            self.retired = Some(epoch);
            self.member_progress.clear();
            self.actions.push(Action::Retired { epoch });
        }
        let Some((epoch, from)) = self.removed_from() else {
            self.member_progress.clear();
            self.retire_probe = None;
            return;
        };
        let members = self.schedule.members(Slot(from));
        let progress = |m: &NodeId| self.member_progress.get(m).copied().unwrap_or(0);
        let caught_up = members.iter().filter(|m| progress(m) >= from).count();
        if caught_up > members.len() / 2 {
            self.retire_probe = None;
            // One at a time: an older one confirms nothing for this epoch when it commits, and
            // a new one follows. Busy or fenced: tried again on the next event.
            if self.retire_barrier.is_none()
                && let Ok(id) = self.submit(Bytes::new(), Kind::Barrier)
            {
                self.retire_barrier = Some((epoch, id));
            }
            return;
        }
        if self.retire_probe.is_none_or(|t| t <= self.now) {
            self.retire_probe = Some(self.now.after(self.cfg.retransmit_interval));
            let ask: Vec<NodeId> = members.iter().copied().filter(|m| progress(m) < from).collect();
            for member in ask {
                self.send(member, Body::CatchUp { from: Slot(self.committed) });
            }
        }
    }

    /// The retirement barrier committed: [`Action::Retired`] is reported when this call has
    /// applied everything it learned, if the replica is still removed from the same epoch.
    pub(super) fn confirm_retired(&mut self) {
        if let Some((epoch, _)) = self.retire_barrier.take() {
            self.retire_committed = Some(epoch);
        }
    }
}

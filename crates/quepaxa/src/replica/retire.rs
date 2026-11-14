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
//!
//! Whether the replica is removed is not state of its own: it is read from the schedule
//! (`Replica::removal`) every time. The barrier is a sequence, [`Phase`]: `Idle → Draining →
//! Confirmed → Retired`, or back to `Idle` once a later epoch makes the barrier moot. Waiting
//! for the new members (`progress`, `probe`) is not part of it: a barrier submitted for an
//! older epoch stays in flight after a newer change, and the wait for that newer epoch runs
//! meanwhile.
//!
//! Proved on a port in `verification/verus/retirement.rs`.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use bytes::Bytes;

use super::Replica;
use crate::api::Action;
use crate::batch::Kind;
use crate::codec::Body;
use crate::schedule::Schedule;
use crate::types::{NodeId, ProposalId, Slot, Timestamp};

/// The latest epoch, which does not include this replica.
#[derive(Clone, Copy, Debug)]
pub(super) struct Removal<'a> {
    pub(super) epoch: u64,
    /// The epoch's first slot.
    pub(super) from: u64,
    pub(super) members: &'a [NodeId],
}

/// Where this replica's own retirement barrier stands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) enum Phase {
    /// No barrier in flight: a member, removed and waiting for the new members, or fenced.
    #[default]
    Idle,
    /// The barrier, submitted while removed from `epoch`, is in flight; never reported to the
    /// caller. Another is not submitted until it resolves, even for a later epoch.
    Draining { epoch: u64, barrier: ProposalId },
    /// That barrier committed. Retirement is decided once everything learned with it is
    /// applied: the same page may add the replica back.
    Confirmed { epoch: u64 },
    /// `Action::Retired` was reported for `epoch`.
    Retired { epoch: u64 },
}

/// What the replica does after [`Retirement::check`].
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Next {
    Nothing,
    /// Report `Action::Retired`.
    Retire {
        epoch: u64,
    },
    /// Submit a barrier for `epoch` and, if it was taken, call [`Retirement::submitted`].
    Submit {
        epoch: u64,
    },
    /// Ask these members, still behind, for a catch-up page.
    Ask(Vec<NodeId>),
}

#[derive(Default)]
#[cfg_attr(feature = "model", derive(Clone, Hash))]
pub(super) struct Retirement {
    phase: Phase,
    /// Durable progress each member of the latest epoch reported in catch-up replies, while
    /// removed from it.
    progress: BTreeMap<NodeId, u64>,
    /// When to ask the members still behind again.
    probe: Option<Timestamp>,
}

impl Retirement {
    #[cfg(test)]
    pub(super) fn phase(&self) -> Phase {
        self.phase
    }

    /// The removal still to retire from: none once retired from that epoch.
    fn pending<'a>(&self, removal: Option<Removal<'a>>) -> Option<Removal<'a>> {
        removal.filter(|r| self.phase != Phase::Retired { epoch: r.epoch })
    }

    pub(super) fn probe_at(&self) -> Option<Timestamp> {
        self.probe
    }

    /// Notes a member's durable progress, from the envelope of a catch-up reply.
    pub(super) fn note_progress(&mut self, removal: Option<Removal<'_>>, member: NodeId, commit_index: u64) {
        if self.pending(removal).is_some() {
            let p = self.progress.entry(member).or_insert(0);
            *p = (*p).max(commit_index);
        }
    }

    /// Run once everything learned so far is applied. `Confirmed` becomes `Retired` if the
    /// replica is still removed from that epoch, `Idle` otherwise. Then, while removed: once a
    /// majority of the members durably know every slot before their epoch, a barrier is due
    /// (one at a time: an older one confirms nothing for this epoch when it commits, and a
    /// new one follows); until then, the members behind are asked on the retransmit timer.
    pub(super) fn check(&mut self, removal: Option<Removal<'_>>, now: Timestamp, interval: u64) -> Next {
        if let Phase::Confirmed { epoch } = self.phase {
            self.phase = Phase::Idle;
            if self.pending(removal).is_some_and(|r| r.epoch == epoch) {
                self.phase = Phase::Retired { epoch };
                self.progress.clear();
                self.probe = None;
                return Next::Retire { epoch };
            }
        }
        let Some(Removal { epoch, from, members }) = self.pending(removal) else {
            self.progress.clear();
            self.probe = None;
            return Next::Nothing;
        };
        let progress = |m: &NodeId| self.progress.get(m).copied().unwrap_or(0);
        let caught_up = members.iter().filter(|m| progress(m) >= from).count();
        if caught_up > members.len() / 2 {
            self.probe = None;
            return match self.phase {
                Phase::Draining { .. } => Next::Nothing,
                _ => Next::Submit { epoch },
            };
        }
        if self.probe.is_none_or(|t| t <= now) {
            self.probe = Some(now.after(interval));
            return Next::Ask(members.iter().copied().filter(|m| progress(m) < from).collect());
        }
        Next::Nothing
    }

    /// The barrier [`Next::Submit`] asked for was taken.
    pub(super) fn submitted(&mut self, epoch: u64, barrier: ProposalId) {
        debug_assert!(matches!(self.phase, Phase::Idle | Phase::Retired { .. }), "{:?}", self.phase);
        self.phase = Phase::Draining { epoch, barrier };
    }

    /// Proposal `id` committed: true if it was the barrier, which becomes `Confirmed`.
    pub(super) fn committed(&mut self, id: ProposalId) -> bool {
        match self.phase {
            Phase::Draining { epoch, barrier } if barrier == id => {
                self.phase = Phase::Confirmed { epoch };
                true
            }
            _ => false,
        }
    }

    /// Fenced: the barrier in flight, if any, is abandoned and returned.
    pub(super) fn fence(&mut self) -> Option<ProposalId> {
        let Phase::Draining { barrier, .. } = self.phase else { return None };
        self.phase = Phase::Idle;
        Some(barrier)
    }
}

/// The latest epoch in `schedule`, if `me` is not a member of it.
fn removal(schedule: &Schedule, me: NodeId) -> Option<Removal<'_>> {
    let (epoch, from) = schedule.epoch();
    let members = schedule.members(Slot(from));
    (epoch > 0 && !members.contains(&me)).then_some(Removal { epoch, from, members })
}

impl Replica {
    /// Notes a member's durable progress, from the envelope of a catch-up reply.
    pub(super) fn note_progress(&mut self, member: NodeId, commit_index: u64) {
        self.retirement.note_progress(removal(&self.schedule, self.me), member, commit_index);
    }

    /// Reports retirement, submits the barrier or asks the members behind, as
    /// [`Retirement::check`] decides. A barrier refused (busy or fenced) is tried again on
    /// the next event.
    pub(super) fn check_retired(&mut self) {
        let removal = removal(&self.schedule, self.me);
        match self.retirement.check(removal, self.now, self.cfg.retransmit_interval) {
            Next::Nothing => {}
            Next::Retire { epoch } => self.actions.push(Action::Retired { epoch }),
            Next::Submit { epoch } => {
                if let Ok(id) = self.submit(Bytes::new(), Kind::Barrier) {
                    self.retirement.submitted(epoch, id);
                }
            }
            Next::Ask(members) => {
                for member in members {
                    self.send(member, Body::CatchUp { from: Slot(self.committed) });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: Timestamp = Timestamp(100);
    const INTERVAL: u64 = 10;
    const MEMBERS: [NodeId; 3] = [NodeId(1), NodeId(2), NodeId(3)];

    fn removed(epoch: u64) -> Option<Removal<'static>> {
        Some(Removal { epoch, from: 64, members: &MEMBERS })
    }

    /// Removed from `epoch` with a majority of its members caught up.
    fn caught_up(epoch: u64) -> Retirement {
        let mut r = Retirement::default();
        for m in &MEMBERS[..2] {
            r.note_progress(removed(epoch), *m, 64);
        }
        r
    }

    fn in_phase(phase: Phase) -> Retirement {
        Retirement { phase, ..Retirement::default() }
    }

    #[test]
    fn idle_and_removed_asks_the_members_behind_on_the_timer() {
        let mut r = Retirement::default();
        r.note_progress(removed(1), NodeId(1), 64);
        assert_eq!(r.check(removed(1), NOW, INTERVAL), Next::Ask(alloc::vec![NodeId(2), NodeId(3)]));
        assert_eq!(r.probe_at(), Some(NOW.after(INTERVAL)));
        assert_eq!(r.check(removed(1), NOW, INTERVAL), Next::Nothing, "not again before the timer");
        assert_eq!(r.phase(), Phase::Idle);
    }

    #[test]
    fn idle_and_caught_up_asks_for_a_barrier() {
        let mut r = caught_up(1);
        assert_eq!(r.check(removed(1), NOW, INTERVAL), Next::Submit { epoch: 1 });
        assert_eq!((r.phase(), r.probe_at()), (Phase::Idle, None), "refused: asked again next time");
    }

    #[test]
    fn idle_and_a_member_forgets_the_progress() {
        let mut r = caught_up(1);
        assert_eq!(r.check(None, NOW, INTERVAL), Next::Nothing);
        assert!(r.progress.is_empty());
        r.note_progress(None, NodeId(1), 64);
        assert!(r.progress.is_empty(), "progress is only kept while removed");
    }

    #[test]
    fn idle_becomes_draining_when_the_barrier_is_taken() {
        let mut r = caught_up(1);
        r.submitted(1, ProposalId(7));
        assert_eq!(r.phase(), Phase::Draining { epoch: 1, barrier: ProposalId(7) });
        assert_eq!(r.check(removed(1), NOW, INTERVAL), Next::Nothing, "one barrier at a time");
    }

    #[test]
    fn retired_becomes_draining_when_removed_from_a_later_epoch() {
        let mut r = in_phase(Phase::Retired { epoch: 1 });
        r.note_progress(removed(2), NodeId(1), 64);
        r.note_progress(removed(2), NodeId(2), 64);
        assert_eq!(r.check(removed(2), NOW, INTERVAL), Next::Submit { epoch: 2 });
        r.submitted(2, ProposalId(8));
        assert_eq!(r.phase(), Phase::Draining { epoch: 2, barrier: ProposalId(8) });
    }

    #[test]
    fn draining_ignores_other_commits() {
        let mut r = in_phase(Phase::Draining { epoch: 1, barrier: ProposalId(7) });
        assert!(!r.committed(ProposalId(6)));
        assert_eq!(r.phase(), Phase::Draining { epoch: 1, barrier: ProposalId(7) });
    }

    #[test]
    fn draining_becomes_confirmed_when_its_barrier_commits() {
        let mut r = in_phase(Phase::Draining { epoch: 1, barrier: ProposalId(7) });
        assert!(r.committed(ProposalId(7)));
        assert_eq!(r.phase(), Phase::Confirmed { epoch: 1 });
    }

    #[test]
    fn draining_becomes_idle_when_fenced() {
        let mut r = in_phase(Phase::Draining { epoch: 1, barrier: ProposalId(7) });
        assert_eq!(r.fence(), Some(ProposalId(7)));
        assert_eq!(r.phase(), Phase::Idle);
        assert_eq!(r.fence(), None);
    }

    #[test]
    fn a_stale_barrier_blocks_a_new_one_but_not_the_wait() {
        // Removed from epoch 1, then from epoch 2 before the barrier for 1 committed.
        let mut r = in_phase(Phase::Draining { epoch: 1, barrier: ProposalId(7) });
        assert_eq!(r.check(removed(2), NOW, INTERVAL), Next::Ask(MEMBERS.to_vec()));
        for m in &MEMBERS[..2] {
            r.note_progress(removed(2), *m, 64);
        }
        assert_eq!(r.check(removed(2), NOW, INTERVAL), Next::Nothing);
        assert_eq!(r.probe_at(), None);
    }

    #[test]
    fn confirmed_becomes_retired_while_still_removed_from_its_epoch() {
        let mut r = caught_up(1);
        r.phase = Phase::Confirmed { epoch: 1 };
        assert_eq!(r.check(removed(1), NOW, INTERVAL), Next::Retire { epoch: 1 });
        assert_eq!((r.phase(), r.probe_at()), (Phase::Retired { epoch: 1 }, None));
        assert!(r.progress.is_empty());
    }

    #[test]
    fn confirmed_becomes_idle_once_added_back() {
        let mut r = in_phase(Phase::Confirmed { epoch: 1 });
        assert_eq!(r.check(None, NOW, INTERVAL), Next::Nothing);
        assert_eq!(r.phase(), Phase::Idle);
    }

    #[test]
    fn confirmed_becomes_idle_once_removed_from_a_later_epoch() {
        let mut r = caught_up(2);
        r.phase = Phase::Confirmed { epoch: 1 };
        assert_eq!(r.check(removed(2), NOW, INTERVAL), Next::Submit { epoch: 2 }, "the next barrier follows");
        assert_eq!(r.phase(), Phase::Idle);
    }

    #[test]
    fn retired_stays_retired_while_its_epoch_is_the_latest() {
        let mut r = in_phase(Phase::Retired { epoch: 1 });
        r.note_progress(removed(1), NodeId(1), 64);
        assert!(r.progress.is_empty());
        assert_eq!(r.check(removed(1), NOW, INTERVAL), Next::Nothing);
        assert!(!r.committed(ProposalId(7)));
        assert_eq!(r.fence(), None);
        assert_eq!(r.phase(), Phase::Retired { epoch: 1 });
    }
}

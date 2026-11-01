//! A follower's watch on its leader: when to stop waiting for it.
//!
//! A follower hands its entries to the leader and hedges an entry the leader leaves
//! uncommitted. The first hedge also makes it suspect the leader: everything already handed
//! to it is hedged at once, and new entries are proposed here, in batches, instead of
//! waiting a hedge delay each. A batch that loses its slot is proposed again here rather
//! than handed back. The suspicion ends when the leader is heard from again, or when the
//! schedule names another leader. A dead leader's own values deciding (its in-flight slots,
//! taken over by others) say nothing about whether it is alive, so they do not end it.
//!
//! Only when this replica proposes changes; any replica may propose in any slot, so none of
//! this affects safety.

use super::Replica;
use crate::types::{NodeId, Slot, Timestamp};

/// What a replica knows about the leader it hands entries to.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct Watch {
    /// The leader this replica suspects, and since when.
    suspect: Option<(NodeId, Timestamp)>,
}

impl Replica {
    /// Whether this replica proposes its own entries itself: it leads, or it suspects the
    /// leader.
    pub(super) fn proposes_directly(&self) -> bool {
        let leader = self.schedule.leader(Slot(self.committed));
        leader == self.me || self.watch.suspect.is_some_and(|(s, _)| s == leader)
    }

    /// The leader left an entry uncommitted for the hedge delay: hedge everything handed to
    /// it and propose directly from now on.
    pub(super) fn suspect_leader(&mut self) {
        let leader = self.schedule.leader(Slot(self.committed));
        if leader == self.me || self.watch.suspect.is_some_and(|(s, _)| s == leader) {
            return;
        }
        self.watch.suspect = Some((leader, self.now));
        for (id, (e, _)) in core::mem::take(&mut self.forwarded) {
            if let Some(o) = self.outstanding.get_mut(&id) {
                o.2 = true;
            }
            self.metrics.hedged += 1;
            self.hedged.push_back(e);
        }
    }

    /// A message arrived from `from`: a suspected leader that speaks is alive.
    pub(super) fn heard_from(&mut self, from: NodeId) {
        if self.watch.suspect.is_some_and(|(s, _)| s == from) {
            self.watch.suspect = None;
        }
    }
}

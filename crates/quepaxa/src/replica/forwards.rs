//! Forwards and their acknowledgements (#231).
//!
//! Every `Forward` carries a number, and the target acknowledges every one it takes: its
//! `ForwardAck` lists the numbers taken since the last (acknowledgements still coalesce,
//! once per eighth of `retransmit_interval`, `ack_forward`). A forward the target refused
//! (its queue was full) or that never arrived is not listed.
//!
//! The origin keeps each forward that carried own entries until it is acknowledged. One left
//! unacknowledged for the resend floor (`resend_wait`: `retransmit_interval`, or twice the
//! smoothed first-step round trip) is sent again to the same target, with the same number
//! and only the entries still waiting on it, backing off as record requests do; the target
//! drops entries already committed, and commit-time dedup filters any other copy. A forward
//! dropped on the wire is so retried within about a round trip, not hedged.
//!
//! An entry counts as acknowledged once the latest forward that carried it is: only then may
//! a busy target's progress hold back its hedge (`late_at`). A forward the target never took
//! cannot be in its backlog.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use super::Replica;
use crate::batch;
use crate::codec::Body;
use crate::types::{NodeId, ProposalId, Timestamp};

/// A forward not yet acknowledged.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "model", derive(Hash))]
struct Unacked {
    to: NodeId,
    /// The own entries it carried.
    ids: Vec<ProposalId>,
    /// How many times it was sent again.
    resends: u32,
    /// When to send it again.
    due: Timestamp,
}

#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct Forwards {
    /// The number of the next forward.
    next: u64,
    /// Forwards not yet acknowledged, by number.
    unacked: BTreeMap<u64, Unacked>,
    /// Each own entry handed to a target: the latest forward that carried it, and whether
    /// that one was acknowledged.
    latest: BTreeMap<ProposalId, (u64, bool)>,
    /// Target side: forwards taken from each member, to acknowledge.
    taken: BTreeMap<NodeId, Vec<u64>>,
}

impl Forwards {
    /// Numbers the next forward, to `to` carrying own entries `ids`, resent at `due` until
    /// acknowledged (if it carries any).
    pub(super) fn sent(&mut self, to: NodeId, ids: Vec<ProposalId>, due: Timestamp) -> u64 {
        let seq = self.next;
        self.next += 1;
        if !ids.is_empty() {
            for id in &ids {
                self.latest.insert(*id, (seq, false));
            }
            self.unacked.insert(seq, Unacked { to, ids, resends: 0, due });
        }
        seq
    }

    /// `from` acknowledged forwards `seqs`.
    pub(super) fn acked(&mut self, from: NodeId, seqs: &[u64]) {
        for seq in seqs {
            if !self.unacked.get(seq).is_some_and(|u| u.to == from) {
                continue;
            }
            let Some(u) = self.unacked.remove(seq) else { continue };
            for id in u.ids {
                if let Some(l) = self.latest.get_mut(&id)
                    && l.0 == *seq
                {
                    l.1 = true;
                }
            }
        }
    }

    /// Whether the latest forward of own entry `id` was acknowledged.
    pub(super) fn is_acked(&self, id: ProposalId) -> bool {
        self.latest.get(&id).is_some_and(|l| l.1)
    }

    /// Own entry `id` is no longer awaited.
    pub(super) fn forget(&mut self, id: ProposalId) {
        self.latest.remove(&id);
    }

    /// Everything is dropped (fenced).
    pub(super) fn clear(&mut self) {
        let next = self.next;
        *self = Self { next, ..Self::default() };
    }

    /// When the next unacknowledged forward is due to be sent again.
    pub(super) fn due(&self) -> Option<Timestamp> {
        self.unacked.values().map(|u| u.due).min()
    }

    /// Target side: forward `seq` from `from` was taken.
    pub(super) fn took(&mut self, from: NodeId, seq: u64) {
        self.taken.entry(from).or_default().push(seq);
    }

    /// Target side: the forwards from `from` to acknowledge now.
    pub(super) fn take_acks(&mut self, from: NodeId) -> Vec<u64> {
        self.taken.remove(&from).unwrap_or_default()
    }
}

impl Replica {
    /// Sends own and handed-on entries `items` to `to` as one forward.
    pub(super) fn send_forward(&mut self, to: NodeId, items: Vec<batch::Item>) {
        let (me, inc) = (self.me, self.incarnation);
        let ids =
            items.iter().filter(|i| i.entry.origin == me && i.entry.incarnation == inc).map(|i| i.entry.id);
        let due = self.now.after(self.resend_wait(None));
        let seq = self.watch.fwd.sent(to, ids.collect(), due);
        self.send(to, Body::Forward { seq, batch: batch::encode(&items) });
        self.metrics.forwarded += 1;
    }

    /// Sends again every forward left unacknowledged past its time, with the entries still
    /// waiting on it; one whose entries all moved on, or whose target is no longer this
    /// replica's, is dropped.
    pub(super) fn resend_forwards(&mut self) {
        let now = self.now;
        let due: Vec<u64> =
            self.watch.fwd.unacked.iter().filter(|(_, u)| u.due <= now).map(|(s, _)| *s).collect();
        let target = (!self.proposes_directly()).then(|| self.target());
        for seq in due {
            let Some(u) = self.watch.fwd.unacked.get(&seq) else { continue };
            let latest = &self.watch.fwd.latest;
            let items: Vec<batch::Item> = u
                .ids
                .iter()
                .filter(|id| latest.get(id).is_some_and(|l| l.0 == seq))
                .filter_map(|id| self.entries.forwarded_item(*id).cloned())
                .collect();
            if items.is_empty() || target != Some(u.to) {
                self.watch.fwd.unacked.remove(&seq);
                continue;
            }
            let (to, k) = (u.to, u.resends);
            let wait = self.resend_wait(Some(k));
            if let Some(u) = self.watch.fwd.unacked.get_mut(&seq) {
                u.resends = k.saturating_add(1);
                u.due = now.after(wait);
            }
            self.send(to, Body::Forward { seq, batch: batch::encode(&items) });
            self.metrics.forward_resends += 1;
        }
    }
}

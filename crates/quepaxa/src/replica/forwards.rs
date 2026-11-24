//! Forwards and their acknowledgements (#231).
//!
//! Every `Forward` carries a number, and the target acknowledges every one it takes: its
//! `ForwardAck` lists the numbers taken since the last (acknowledgements still coalesce,
//! once per eighth of `retransmit_interval`, `ack_forward`). A forward the target refused
//! (its queue was full) or that never arrived is not listed.
//!
//! The origin keeps each forward that carried own entries until it is acknowledged. One left
//! unacknowledged for the resend floor (`forward_wait`) is sent again to the same target,
//! with the same number and only the entries still waiting on it, backing off as record
//! requests do; the target drops entries already committed, and commit-time dedup filters
//! any other copy. A forward dropped on the wire is so retried, not hedged.
//!
//! The floor follows the delay from sending a forward to its acknowledgement, kept per
//! target (`ack_delay`). Acknowledgements leave the target on the link its decisions fill,
//! so under saturation they arrive late; a floor taken from any other message, or from this
//! replica's own round trips, resent forwards still on their way and spent the link on them
//! (#231). An acknowledgement of a forward sent once is a sample; one of a forward sent
//! again may answer either copy, and only raises the delay (to the time since the last
//! copy), so a floor that fell behind the link cannot stay there.
//!
//! An entry counts as acknowledged once the latest forward that carried it is: only then may
//! a busy target's progress hold back its hedge (`late_at`). A forward the target never took
//! cannot be in its backlog.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use super::Replica;
use crate::batch::{self, Piece, Ref};
use crate::codec::Body;
use crate::types::{NodeId, ProposalId, Timestamp};

/// A forward not yet acknowledged.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "model", derive(Hash))]
struct Unacked {
    to: NodeId,
    /// The own entries it carried.
    ids: Vec<ProposalId>,
    /// Data-batch references for this forward when it carried own entries by reference
    /// (`inline_batch_bytes < max_batch_bytes`), empty for inline forwards.
    refs: Vec<Ref>,
    /// How many times it was sent again.
    resends: u32,
    /// When it was last sent.
    sent_at: Timestamp,
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
    /// Smoothed delay from sending a forward to its acknowledgement, per target, in ns.
    pub(super) ack_delay: BTreeMap<NodeId, u64>,
}

impl Forwards {
    /// Numbers the next forward, to `to` carrying own entries `ids`, resent at `due` until
    /// acknowledged (if it carries any).
    pub(super) fn sent(
        &mut self,
        to: NodeId,
        ids: Vec<ProposalId>,
        refs: Vec<Ref>,
        now: Timestamp,
        due: Timestamp,
    ) -> u64 {
        let seq = self.next;
        self.next += 1;
        if !ids.is_empty() {
            for id in &ids {
                self.latest.insert(*id, (seq, false));
            }
            self.unacked.insert(seq, Unacked { to, ids, refs, resends: 0, sent_at: now, due });
        }
        seq
    }

    /// `from` acknowledged forwards `seqs` at `now`.
    pub(super) fn acked(&mut self, from: NodeId, seqs: &[u64], now: Timestamp) {
        for seq in seqs {
            if !self.unacked.get(seq).is_some_and(|u| u.to == from) {
                continue;
            }
            let Some(u) = self.unacked.remove(seq) else { continue };
            let sample = now.0.saturating_sub(u.sent_at.0);
            let avg = self.ack_delay.entry(from).or_insert(sample);
            // Rises at once to a slower sample; decays by an eighth toward a faster one, and
            // only on a forward sent once (an acknowledgement of a resend may answer either
            // copy, so it is a lower bound at best).
            *avg = if sample >= *avg {
                sample
            } else if u.resends == 0 {
                *avg - *avg / 8 + sample / 8
            } else {
                *avg
            };
            for id in u.ids {
                if let Some(l) = self.latest.get_mut(&id)
                    && l.0 == *seq
                {
                    l.1 = true;
                }
            }
        }
    }

    /// The smoothed acknowledgement delay of `to`, once one was seen.
    pub(super) fn ack_delay(&self, to: NodeId) -> Option<u64> {
        self.ack_delay.get(&to).copied()
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
        let (next, ack_delay) = (self.next, core::mem::take(&mut self.ack_delay));
        *self = Self { next, ack_delay, ..Self::default() };
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
    /// How long to wait for `to` to acknowledge a forward before sending it again, after
    /// `resent` earlier resends (`None`: a first send): twice the acknowledgement delay, or
    /// that delay plus the acknowledgement cooldown, never below `retransmit_interval`;
    /// `hedge_max` until an acknowledgement was seen. Each resend waits twice as long as the
    /// one before, up to `hedge_max` (or the floor, if that is longer).
    pub(super) fn forward_wait(&self, to: NodeId, resent: Option<u32>) -> u64 {
        let (retransmit, cap) = (self.cfg.retransmit_interval, self.cfg.hedge_max);
        let floor = self
            .watch
            .fwd
            .ack_delay(to)
            .map_or(cap, |d| d.saturating_mul(2).max(d.saturating_add(self.ack_every())).max(retransmit));
        let wait = resent.map_or(floor, |k| floor.saturating_mul(1 << (k.min(31) + 1)));
        wait.min(cap.max(floor))
    }

    /// Sends `pieces` to `to` as one forward, tracking `own_ids` (and any originated `refs`)
    /// until acknowledged.
    pub(super) fn send_forward(
        &mut self,
        to: NodeId,
        pieces: Vec<Piece>,
        own_ids: Vec<ProposalId>,
        refs: Vec<Ref>,
    ) {
        let (now, due) = (self.now, self.now.after(self.forward_wait(to, None)));
        let seq = self.watch.fwd.sent(to, own_ids, refs, now, due);
        self.send(to, Body::Forward { seq, batch: batch::encode_pieces(&pieces) });
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
            let refs = u.refs.clone();
            let wait = self.forward_wait(to, Some(k));
            if let Some(u) = self.watch.fwd.unacked.get_mut(&seq) {
                u.resends = k.saturating_add(1);
                (u.sent_at, u.due) = (now, now.after(wait));
            }
            let batch = if !refs.is_empty() && refs.iter().all(|r| self.content.check(*r).is_ok()) {
                let pieces: Vec<Piece> = refs.into_iter().map(Piece::Ref).collect();
                batch::encode_pieces(&pieces)
            } else {
                batch::encode(&items)
            };
            self.metrics.forward_resends += 1;
            self.metrics.forward_resent_bytes += batch.len() as u64;
            self.send(to, Body::Forward { seq, batch });
        }
    }
}

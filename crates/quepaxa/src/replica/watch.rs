//! A follower's watch on the replica it hands its entries to: when to stop waiting for it.
//!
//! A follower hands its entries to its target: the first member of the leader order it does
//! not suspect, normally the leader. When an entry handed over stays uncommitted for the
//! hedge delay, the follower suspects the target and hands everything still waiting to the
//! next member instead: the deputy, the one the schedule would rotate to. A deputy proposes
//! what it is handed itself; when the deputy is this replica, it proposes its own entries
//! too, and a batch of them that loses its slot is proposed again rather than handed back.
//! One proposer at a time, so followers do not contend for slots while the schedule has not
//! rotated yet. A suspected replica that speaks is alive again, and the schedule naming a
//! new leader moves the target with it. A dead leader's own values deciding (its in-flight
//! slots, taken over by others) say nothing about whether it is alive.
//!
//! A target that says nothing at all is not waited for as long. An alive one that got a
//! forward sends this replica something (a record request at least) within R, the usual
//! delay from a forward to its first message, which takes in the leader's own durability
//! waits. So once σ = max(commit latency, 2R), within `[hedge_min, δ]`, passes with no word
//! from the target since an entry was forwarded, the entry counts as late; and a slot whose
//! leader is suspected, or silent since the slot was seen, is taken over after
//! `position · σ` instead of `(position + 1) · δ`. A slow target still talks and is waited
//! for as before.
//!
//! Only who proposes, and when, changes; any replica may propose in any slot, so none of
//! this affects safety.

use alloc::collections::BTreeMap;

use super::Replica;
use crate::types::{NodeId, Slot, Timestamp};

/// What a replica knows about the members it hands entries to.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct Watch {
    /// Members this replica suspects, since when.
    pub(super) suspects: BTreeMap<NodeId, Timestamp>,
    /// When a message last arrived from each member.
    pub(super) heard: BTreeMap<NodeId, Timestamp>,
    /// The first forward the target has not said anything since.
    pub(super) asked: Option<Timestamp>,
    /// Smoothed delay from a forward to the target's next message (R), in nanoseconds.
    pub(super) reply: Option<u64>,
}

impl Replica {
    /// Who this replica hands its entries to: the first member of the leader order it does
    /// not suspect (itself when it comes first).
    pub(super) fn target(&self) -> NodeId {
        let order = self.schedule.members(Slot(self.committed));
        order
            .iter()
            .copied()
            .find(|n| *n == self.me || !self.watch.suspects.contains_key(n))
            .unwrap_or(self.me)
    }

    /// Whether this replica proposes its own entries itself.
    pub(super) fn proposes_directly(&self) -> bool {
        self.target() == self.me
    }

    /// An entry handed over stayed uncommitted: suspect the target and hand everything still
    /// waiting to the next member (or propose it here, if the target is now this replica).
    pub(super) fn suspect_target(&mut self) {
        let target = self.target();
        if target != self.me {
            self.watch.suspects.insert(target, self.now);
        }
        self.watch.asked = None;
        for (id, (e, _)) in core::mem::take(&mut self.forwarded).into_iter().rev() {
            if let Some(o) = self.outstanding.get_mut(&id) {
                o.2 = true;
            }
            self.metrics.hedged += 1;
            self.pending.push_front(e);
        }
    }

    /// A message arrived from `from`: a suspected member that speaks is alive.
    pub(super) fn heard_from(&mut self, from: NodeId) {
        self.watch.suspects.remove(&from);
        self.watch.heard.insert(from, self.now);
        if from == self.target()
            && let Some(at) = self.watch.asked.take()
        {
            let sample = self.now.0.saturating_sub(at.0);
            self.watch.reply = Some(self.watch.reply.map_or(sample, |avg| avg - avg / 8 + sample / 8));
        }
    }

    /// Entries were just handed to the target.
    pub(super) fn forwarded_now(&mut self) {
        self.watch.asked.get_or_insert(self.now);
    }

    /// σ, once both the commit latency and R have been observed.
    pub(super) fn silence_wait(&self) -> Option<u64> {
        let (latency, reply) = (self.commit_latency?, self.watch.reply?);
        Some(latency.max(reply.saturating_mul(2)).clamp(self.cfg.hedge_min, self.hedge_delay()))
    }

    /// Whether `node` has said nothing since `t`.
    fn silent_since(&self, node: NodeId, t: Timestamp) -> bool {
        self.watch.heard.get(&node).is_none_or(|h| *h <= t)
    }

    /// When an entry forwarded at `at` counts as late.
    pub(super) fn hedge_at(&self, at: Timestamp) -> Timestamp {
        match self.silence_wait() {
            Some(sigma) if self.silent_since(self.target(), at) => at.after(sigma),
            _ => at.after(self.hedge_delay()),
        }
    }

    /// When to take over `slot`, seen at `seen`, from whoever started it.
    pub(super) fn takeover_at(&self, slot: Slot, seen: Timestamp) -> Timestamp {
        let position = self.schedule.position(self.me, slot);
        let leader = self.schedule.leader(slot);
        let suspected = self.watch.suspects.contains_key(&leader);
        match self.silence_wait() {
            Some(sigma) if leader != self.me && (suspected || self.silent_since(leader, seen)) => {
                seen.after(position.max(1).saturating_mul(sigma))
            }
            _ => seen.after((position + 1).saturating_mul(self.hedge_delay())),
        }
    }
}

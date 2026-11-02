//! A follower's watch on the replica it hands its entries to: when to stop waiting for it.
//!
//! A follower hands its entries to its target: the first member of the leader order it does
//! not suspect, normally the leader. An entry the target leaves uncommitted for the hedge
//! delay δ is proposed here, as before; the target is still talking, only slow. A target
//! that says nothing at all, though, is suspected, and the follower hands everything still
//! waiting to the next member instead: the deputy, the one the schedule would rotate to. A deputy proposes
//! what it is handed itself; when the deputy is this replica, it proposes its own entries
//! too, and a batch of them that loses its slot is proposed again rather than handed back.
//! One proposer at a time, so followers do not contend for slots while the schedule has not
//! rotated yet. A suspected replica that speaks is alive again, and the schedule naming a
//! new leader moves the target with it. A dead leader's own values deciding (its in-flight
//! slots, taken over by others) say nothing about whether it is alive.
//!
//! Silence is judged against R, the delay from a forward to the target's next message. A
//! target acknowledges forwards at once, before any write (`ack_forward`), so R is about a
//! round trip even while its disk is slow; R rises at once to a slower sample and decays slowly. Once
//! σ = max(commit latency, 2R, `hedge_min`) passes with no word from the target since an
//! entry was forwarded, the target is suspected; and a slot is taken over after
//! `position · σ` instead of `(position + 1) · δ` once its leader has said nothing for σ (a
//! busy leader whose slots queue keeps talking, and is waited for). Only while σ < δ:
//! otherwise silence is no earlier a sign than the hedge delay, and the follower behaves as
//! without it. A slow target still talks and is waited for as before.
//!
//! Silence counts only with `Config::silent_leader_takeover`; without it a follower waits
//! the hedge delay and hedges entry by entry, as before.
//!
//! Only who proposes, and when, changes; any replica may propose in any slot, so none of
//! this affects safety.

use alloc::collections::BTreeMap;

use super::Replica;
use crate::codec::Body;
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
    /// When this replica last acknowledged a forward from each member.
    pub(super) acked: BTreeMap<NodeId, Timestamp>,
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
            self.watch.reply = Some(match self.watch.reply {
                Some(avg) if sample < avg => avg - avg / 8 + sample / 8,
                _ => sample,
            });
        }
    }

    /// Acknowledges a forward before any write, so a leader waiting on its disk still shows
    /// it is alive; at most once per eighth of `retransmit_interval` per member. Every
    /// message is a packet a lossy link can lose, stalling the stream behind it: an ack per
    /// forward tripled the p99 across regions with 1% loss on the leader's links. R takes
    /// in the wait, so σ covers it.
    pub(super) fn ack_forward(&mut self, from: NodeId) {
        let every = self.cfg.retransmit_interval / 8;
        if self.watch.acked.get(&from).is_none_or(|t| self.now.0 >= t.0.saturating_add(every)) {
            self.watch.acked.insert(from, self.now);
            self.send(from, Body::ForwardAck);
        }
    }

    /// Entries were just handed to the target.
    pub(super) fn forwarded_now(&mut self) {
        self.watch.asked.get_or_insert(self.now);
    }

    /// σ, once both the commit latency and R have been observed, and only while it is
    /// shorter than δ: silence that cannot be judged before δ adds nothing to it.
    pub(super) fn silence_wait(&self) -> Option<u64> {
        if !self.cfg.silent_leader_takeover {
            return None;
        }
        let (latency, reply) = (self.commit_latency?, self.watch.reply?);
        let floor = self.cfg.hedge_min;
        let sigma = latency.max(reply.saturating_mul(2)).max(floor);
        (sigma < self.hedge_delay()).then_some(sigma)
    }

    /// Whether `node` has said nothing since `t`.
    fn silent_since(&self, node: NodeId, t: Timestamp) -> bool {
        self.watch.heard.get(&node).is_none_or(|h| *h <= t)
    }

    /// When the target has been silent for σ since `at`, if it has.
    pub(super) fn silent_at(&self, at: Timestamp) -> Option<Timestamp> {
        let sigma = self.silence_wait()?;
        self.silent_since(self.target(), at).then(|| at.after(sigma))
    }

    /// When an entry forwarded at `at` is acted on: hedged after δ, or sooner, with its
    /// target suspected, if the target stays silent.
    pub(super) fn hedge_at(&self, at: Timestamp) -> Timestamp {
        let delta = at.after(self.hedge_delay());
        self.silent_at(at).map_or(delta, |s| s.min(delta))
    }

    /// When to take over `slot`, seen at `seen`, from whoever started it.
    pub(super) fn takeover_at(&self, slot: Slot, seen: Timestamp) -> Timestamp {
        let position = self.schedule.position(self.me, slot);
        let leader = self.schedule.leader(slot);
        let usual = seen.after((position + 1).saturating_mul(self.hedge_delay()));
        match self.silence_wait() {
            // Sooner only once the leader has been silent for σ: a leader still talking
            // (busy, its slots queued) keeps pushing this back.
            Some(sigma) if leader != self.me => {
                let silent = self.watch.heard.get(&leader).map_or(seen, |h| (*h).max(seen)).after(sigma);
                seen.after(position.max(1).saturating_mul(sigma)).max(silent).min(usual)
            }
            _ => usual,
        }
    }
}

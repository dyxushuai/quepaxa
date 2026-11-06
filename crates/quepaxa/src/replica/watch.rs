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
//! Silence is judged against R, the delay from a forward to a member's next message, kept per
//! member: a deputy may be farther than the leader. A member acknowledges forwards at once,
//! before any write (`ack_forward`), so R is about a round trip even while its disk is slow;
//! R rises at once to a slower sample and decays slowly. Once σ = max(commit latency, 2R, R +
//! the acknowledgement cooldown, `hedge_min`) passes with no word from the target since an
//! entry was forwarded to it, the target is suspected; and a slot is taken over after
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

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

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
    /// The first forward the member it went to has not said anything since.
    pub(super) asked: Option<(NodeId, Timestamp)>,
    /// Current forwarding destination and when it first received entries in this run.
    pub(super) forwarded_to: Option<(NodeId, Timestamp)>,
    /// Smoothed delay from a forward to each member's next message (R), in nanoseconds.
    pub(super) reply: BTreeMap<NodeId, u64>,
    /// Persists issued and not yet durable: id and when.
    pub(super) unpersisted: VecDeque<(u64, Timestamp)>,
    /// Smoothed time from issuing a persist to its being durable, in nanoseconds.
    pub(super) persist: Option<u64>,
    /// When another replica last told this one of a slot it decided.
    pub(super) decided_elsewhere: Timestamp,
    /// This replica stepped aside (`aside_at`) and handed its batches to the next member.
    pub(super) aside: bool,
    /// When the target last announced a slot it decided.
    pub(super) progressed: Timestamp,
    /// When this replica last acknowledged a forward from each member.
    pub(super) acked: BTreeMap<NodeId, Timestamp>,
    /// Coalesced acknowledgements waiting for each member's cooldown to expire.
    pub(super) ack_due: BTreeMap<NodeId, Timestamp>,
}

impl Replica {
    /// Who this replica hands its entries to: the first member of the leader order it does
    /// not suspect (itself when it comes first).
    pub(super) fn target(&self) -> NodeId {
        let order = self.schedule.members(Slot(self.committed));
        let aside = self.stepped_aside();
        order
            .iter()
            .copied()
            .find(|n| if *n == self.me { !aside } else { !self.watch.suspects.contains_key(n) })
            .unwrap_or(self.me)
    }

    /// When this replica steps aside: once a write has waited on its disk for 4δ (at most
    /// `hedge_max`), and for four times as long as a write usually takes, if another replica
    /// has decided a slot since it was issued: the others can go on without this disk (when
    /// every disk is stuck, stepping aside would only add contention once they resume). Its slots cannot progress while its disk is stuck, so it hands its own
    /// entries to the next member of the leader order and stops acknowledging forwards,
    /// which sends the other followers there too. Back as soon as the disk catches up.
    pub(super) fn aside_at(&self) -> Option<Timestamp> {
        if !self.cfg.silent_leader_takeover || self.schedule.members(Slot(self.committed)).len() < 2 {
            return None;
        }
        let (d, usual) = (self.hedge_delay(), self.watch.persist?);
        let (_, oldest) = self.watch.unpersisted.front()?;
        if self.watch.decided_elsewhere <= *oldest {
            return None;
        }
        Some(oldest.after(d.saturating_mul(4).min(self.cfg.hedge_max).max(d).max(usual.saturating_mul(4))))
    }

    pub(super) fn stepped_aside(&self) -> bool {
        self.aside_at().is_some_and(|t| t <= self.now)
    }

    /// On stepping aside, hands the entries of every local batch not yet decided to the
    /// next member (a batch already in a slot stays there too: whichever copy is decided
    /// first commits, the other is filtered as a duplicate).
    pub(super) fn check_aside(&mut self) {
        let aside = self.stepped_aside();
        if aside == self.watch.aside {
            return;
        }
        self.watch.aside = aside;
        if !aside {
            return;
        }
        let mut handed = Vec::new();
        self.batches.retain(|_, b| {
            // Our own encoding: it decodes.
            handed.extend(crate::batch::decode(&b.value).unwrap_or_default());
            b.slot.is_some()
        });
        let dedup = &self.dedup;
        self.pending.extend(handed.into_iter().filter(|e| {
            let (o, inc, id) = e.key();
            !dedup.contains(o, inc, id)
        }));
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
        if let Some((to, at)) = self.watch.asked
            && to == from
        {
            self.watch.asked = None;
            let sample = self.now.0.saturating_sub(at.0);
            let avg = self.watch.reply.entry(from).or_insert(sample);
            *avg = if sample < *avg { *avg - *avg / 8 + sample / 8 } else { sample };
        }
    }

    /// How long a forward may wait for its acknowledgement after an earlier one was sent.
    fn ack_every(&self) -> u64 {
        self.cfg.retransmit_interval / 8
    }

    /// Acknowledges a forward before any write, so a leader waiting on its disk still shows
    /// it is alive; at most once per eighth of `retransmit_interval` per member. Every
    /// message is a packet a lossy link can lose, stalling the stream behind it: an ack per
    /// forward tripled the p99 across regions with 1% loss on the leader's links. Suppressed
    /// acknowledgements coalesce until the cooldown ends; σ covers that wait.
    pub(super) fn ack_forward(&mut self, from: NodeId) {
        if self.stepped_aside() {
            self.watch.ack_due.remove(&from);
            return;
        }
        let every = self.ack_every();
        if let Some(due) = self.watch.acked.get(&from).map(|t| t.after(every))
            && self.now < due
        {
            self.watch.ack_due.insert(from, due);
            return;
        }
        self.watch.ack_due.remove(&from);
        self.watch.acked.insert(from, self.now);
        self.send(from, Body::ForwardAck);
    }

    /// Entries were just handed to the target.
    pub(super) fn forwarded_now(&mut self) {
        let target = self.target();
        if self.watch.forwarded_to.is_none_or(|(to, _)| to != target) {
            self.watch.forwarded_to = Some((target, self.now));
        }
        if self.watch.asked.is_none_or(|(to, _)| to != target) {
            self.watch.asked = Some((target, self.now));
        }
    }

    /// σ for `node`, once the commit latency and its R have been observed, and only while it
    /// is shorter than δ: silence that cannot be judged before δ adds nothing to it. It
    /// covers twice R, and R plus the wait for a rate-limited acknowledgement (`ack_forward`).
    pub(super) fn silence_wait(&self, node: NodeId) -> Option<u64> {
        if !self.cfg.silent_leader_takeover {
            return None;
        }
        let (latency, reply) = (self.commit_latency?, *self.watch.reply.get(&node)?);
        let answer = reply.saturating_mul(2).max(reply.saturating_add(self.ack_every()));
        let sigma = latency.max(answer).max(self.cfg.hedge_min);
        (sigma < self.hedge_delay()).then_some(sigma)
    }

    /// Whether `node` has said nothing since `t`.
    fn silent_since(&self, node: NodeId, t: Timestamp) -> bool {
        self.watch.heard.get(&node).is_none_or(|h| *h <= t)
    }

    /// When the target has been silent for σ since `at`, if it has.
    pub(super) fn silent_at(&self, at: Timestamp) -> Option<Timestamp> {
        let target = self.target();
        let (to, since) = self.watch.forwarded_to?;
        if to != target {
            return None;
        }
        let at = at.max(since);
        let sigma = self.silence_wait(target)?;
        self.silent_since(target, at).then(|| at.after(sigma))
    }

    /// When an entry forwarded at `at` is hedged although its target talks: after δ, or
    /// after 4δ (at most `hedge_max`) if the target has decided a slot itself since (it is
    /// working through a backlog, and hedges would only contend with it; still hedged, in
    /// case the forward was lost). A slot others took over from it does not count.
    pub(super) fn late_at(&self, at: Timestamp) -> Timestamp {
        let d = self.hedge_delay();
        let busy = d.saturating_mul(4).min(self.cfg.hedge_max).max(d);
        at.after(if self.watch.progressed > at { busy } else { d })
    }

    /// When an entry forwarded at `at` is acted on: hedged when late, or sooner, with its
    /// target suspected, if the target stays silent.
    pub(super) fn hedge_at(&self, at: Timestamp) -> Option<Timestamp> {
        // Stepped aside, this replica's own slots are stuck: it hedges nothing here.
        let late = (!self.watch.aside).then(|| self.late_at(at));
        self.silent_at(at).into_iter().chain(late).min()
    }

    /// When to take over `slot`, seen at `seen`, from whoever started it.
    pub(super) fn takeover_at(&self, slot: Slot, seen: Timestamp) -> Timestamp {
        let position = self.schedule.position(self.me, slot);
        let leader = self.schedule.leader(slot);
        let usual = seen.after((position + 1).saturating_mul(self.hedge_delay()));
        match self.silence_wait(leader) {
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

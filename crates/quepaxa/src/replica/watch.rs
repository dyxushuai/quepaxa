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

use alloc::collections::BTreeMap;

use super::Replica;
use super::disk::{Disk, Health};
use crate::codec::Body;
use crate::types::{NodeId, ProposalId, Slot, Timestamp};

/// What a replica knows about the members it hands entries to.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct Watch {
    /// Members this replica suspects, since when.
    pub(super) suspects: BTreeMap<NodeId, Timestamp>,
    /// When a message last arrived from each member.
    pub(super) heard: BTreeMap<NodeId, Timestamp>,
    /// The commit index each member's latest message carried.
    pub(super) committed: BTreeMap<NodeId, u64>,
    /// The first forward the member it went to has not said anything since.
    pub(super) asked: Option<(NodeId, Timestamp)>,
    /// Current forwarding destination and when it first received entries in this run.
    pub(super) forwarded_to: Option<(NodeId, Timestamp)>,
    /// Destination and timestamp of the most recent forward sent, for follower coalescing.
    pub(super) last_forward: Option<(NodeId, Timestamp)>,
    /// Smoothed delay from a forward to each member's next message (R), in nanoseconds.
    pub(super) reply: BTreeMap<NodeId, u64>,
    /// This replica's disk (`disk.rs`).
    pub(super) disk: Disk,
    /// This replica stepped aside (`aside_at`) and handed its batches to the next member;
    /// changed only by `check_aside`.
    pub(super) aside: bool,
    /// When the target last announced a slot it decided.
    pub(super) progressed: Timestamp,
    /// When this replica last acknowledged a forward from each member.
    pub(super) acked: BTreeMap<NodeId, Timestamp>,
    /// Coalesced acknowledgements waiting for each member's cooldown to expire.
    pub(super) ack_due: BTreeMap<NodeId, Timestamp>,
    /// Forwards sent and not yet acknowledged, and those to acknowledge (`forwards.rs`).
    pub(super) fwd: super::forwards::Forwards,
}

impl Replica {
    /// Who this replica hands its entries to: the first member of the leader order it does
    /// not suspect (itself when it comes first).
    pub(super) fn target(&self) -> NodeId {
        let first = self.first_unsuspected();
        if first == self.me && self.stepped_aside() { self.next_after_me() } else { first }
    }

    /// The first member of the leader order this replica does not suspect (itself when it
    /// comes first).
    fn first_unsuspected(&self) -> NodeId {
        let order = self.schedule.routing_order(Slot(self.committed));
        order
            .iter()
            .copied()
            .find(|n| *n == self.me || !self.watch.suspects.contains_key(n))
            .unwrap_or(self.me)
    }

    /// The first unsuspected member after this one in the leader order (itself if none).
    fn next_after_me(&self) -> NodeId {
        let order = self.schedule.routing_order(Slot(self.committed));
        order
            .iter()
            .copied()
            .skip_while(|n| *n != self.me)
            .skip(1)
            .find(|n| !self.watch.suspects.contains_key(n))
            .unwrap_or(self.me)
    }

    /// When this replica, if others hand it their entries, steps aside: once a write has
    /// waited on its disk for 4δ (at most `hedge_max`, and at least four usual write times),
    /// if another replica has decided a slot since it was issued. Its slots cannot progress
    /// while its disk is stuck, so it hands its own entries to the next member of the leader
    /// order and stops acknowledging forwards, which sends the followers there too; back as
    /// soon as the disk catches up. Not when nobody else decides either (every disk stuck,
    /// one shared disk): that would only add contention once the disks resume.
    pub(super) fn aside_at(&self) -> Option<Timestamp> {
        // Only the replica others hand their entries to: a follower's slots hold nobody up.
        if !self.cfg.silent_leader_takeover
            || self.first_unsuspected() != self.me
            || self.next_after_me() == self.me
        {
            return None;
        }
        self.watch.disk.alone_at(self.busy_delay())
    }

    /// 4δ, at most `hedge_max`, at least δ.
    fn busy_delay(&self) -> u64 {
        let d = self.hedge_delay();
        d.saturating_mul(4).min(self.cfg.hedge_max).max(d)
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
        self.metrics.stepped_aside += 1;
        // What others handed this replica goes on too (`on_forward` routes later forwards).
        self.entries.step_aside(&self.dedup);
    }

    /// Whether this replica proposes its own entries itself.
    pub(super) fn proposes_directly(&self) -> bool {
        self.target() == self.me
    }

    /// Whether the current leader has answered recently enough that a non-direct, non-deputy
    /// proposal is a hedge around a live leader rather than a takeover of a silent one (#258).
    pub(super) fn leader_talking(&self) -> bool {
        let leader = self.schedule.routing_order(Slot(self.committed))[0];
        if leader == self.me || self.watch.suspects.contains_key(&leader) {
            return false;
        }
        if let Some((to, at)) = self.watch.asked
            && to == leader
            && self.now.0.saturating_sub(at.0) >= self.hedge_delay()
        {
            return false;
        }
        self.watch.heard.get(&leader).is_some_and(|&h| self.now.0.saturating_sub(h.0) <= self.busy_delay())
    }

    /// An entry handed over stayed uncommitted: suspect the target and hand everything still
    /// waiting to the next member (or propose it here, if the target is now this replica).
    /// Keep the unanswered forward's timestamp in `suspects` so that if the target was only
    /// slowed behind a saturated link, hearing from it still raises R (#231).
    pub(super) fn suspect_target(&mut self) {
        let target = self.target();
        let asked = self.watch.asked.take().filter(|(to, _)| *to == target).map_or(self.now, |(_, at)| at);
        if target != self.me {
            self.watch.suspects.insert(target, asked);
            self.metrics.suspicions += 1;
        }
        self.metrics.hedged += self.entries.suspect();
    }

    /// A message arrived from `from`: a suspected member that speaks is alive, and its
    /// delayed reply still updates R (capped at `hedge_max`, so a long partition decays
    /// promptly once it heals).
    pub(super) fn heard_from(&mut self, from: NodeId) {
        let suspected = self.watch.suspects.remove(&from);
        self.watch.heard.insert(from, self.now);
        let at = match self.watch.asked {
            Some((to, at)) if to == from => {
                self.watch.asked = None;
                Some(at)
            }
            _ => suspected,
        };
        if let Some(at) = at {
            let sample = self.now.0.saturating_sub(at.0).min(self.cfg.hedge_max);
            let avg = self.watch.reply.entry(from).or_insert(sample);
            *avg = if sample < *avg { *avg - *avg / 8 + sample / 8 } else { sample };
        }
    }

    /// How long a follower coalesces subsequent forwards to `target` after sending one:
    /// an eighth of the observed network forward-ack/reply delay (discounting the leader's
    /// `ack_every` coalescing cooldown so low-RTT LAN clusters do not stretch `4δ` past disk
    /// stalls), capped at half `ack_every`. Uses `ack_delay` alongside `reply` so unrelated
    /// consensus messages from `target` cannot collapse coalescing to zero under steady WAN
    /// load; on cold start while multiple forwards are already unacknowledged and pending data
    /// would disseminate as `Body::Data`, coalesces up to half `ack_every` so large writes do
    /// not flood one `Data` frame per entry.
    pub(super) fn forward_coalesce(&self, target: NodeId) -> u64 {
        let cap = self.ack_every() / 2;
        let ack_net = self.watch.fwd.ack_delay(target).map(|d| d.saturating_sub(self.ack_every()));
        let rtt = ack_net.max(self.watch.reply.get(&target).copied());
        if let Some(r) = rtt {
            return (r / 8).min(cap);
        }
        if self.watch.fwd.unacked_count(target) >= 2
            && self.cfg.inline_batch_bytes < self.cfg.max_batch_bytes
            && self.entries.pending_exceeds(self.cfg.inline_batch_bytes as usize)
        {
            return cap;
        }
        0
    }

    /// When held follower entries should flush to the target, if coalescing holds them.
    pub(super) fn forward_flush_at(&self) -> Option<Timestamp> {
        if self.proposes_directly() || !self.entries.has_pending() {
            return None;
        }
        let target = self.target();
        let wait = self.forward_coalesce(target);
        if wait == 0 {
            return None;
        }
        let (to, at) = self.watch.last_forward?;
        (to == target).then(|| at.after(wait)).filter(|&due| self.now < due)
    }

    /// How long a forward may wait for its acknowledgement after an earlier one was sent.
    pub(super) fn ack_every(&self) -> u64 {
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
            let _dropped = self.watch.fwd.take_acks(from);
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
        let seqs = self.watch.fwd.take_acks(from);
        self.send(from, Body::ForwardAck { seqs });
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

    /// When an entry forwarded at `at` is hedged although its target talks: δ after the
    /// forward or, once the target has decided a slot itself since, 4δ (at most `hedge_max`)
    /// later. That 4δ counts from the target's last decision once it acknowledged the forward
    /// (`acked`, `forwards.rs`): a target still deciding is working through a backlog, and
    /// hedges only add to it; on a saturated link they took a third of the leader's egress and
    /// halved throughput (#231). From the forward otherwise: a forward the target never took
    /// is in no backlog (it is also sent again, `resend_forwards`). Whatever the target's
    /// progress, the entry is hedged 4 · `hedge_max` after the forward at the latest (the
    /// returned flag: that bound set the time). A target that falls silent is suspected after
    /// σ. A slot others took over from it does not count.
    ///
    /// Not while this replica's disk is stuck with nobody deciding (`Health::SharedStall`), and
    /// counted from when it caught up: no slot can be decided meanwhile, and hedges made then
    /// would contend with the target's slots the moment the disks resume, pushing them off
    /// the fast path. Stepped aside, it hands them over again as usual.
    ///
    /// Held only once the disk is stuck, not when it would be by `late` if no write completed
    /// meanwhile: a slow disk keeps completing writes and pushing that forecast back, and an
    /// entry held on it had no timer until something else woke this replica.
    pub(super) fn late(&self, at: Timestamp, acked: bool) -> Option<(Timestamp, bool)> {
        let d = self.hedge_delay();
        let (progressed, resumed) = (self.watch.progressed, self.watch.disk.resumed());
        let busy = progressed > at;
        let from = if busy && acked { progressed } else { at };
        let wait = if busy { self.busy_delay() } else { d };
        let unbounded = from.max(resumed).after(wait);
        let bound = at.max(resumed).after(self.cfg.hedge_max.saturating_mul(4));
        let late = unbounded.min(bound);
        let held = !self.watch.aside
            && matches!(self.watch.disk.health(self.now, d), Health::SharedStall { since } if since <= late);
        (!held).then_some((late, bound < unbounded))
    }

    /// `late` without the flag.
    #[cfg(test)]
    pub(super) fn late_at(&self, at: Timestamp, acked: bool) -> Option<Timestamp> {
        self.late(at, acked).map(|(t, _)| t)
    }

    /// `late` for own entry `id`, forwarded at `at`.
    pub(super) fn lateness(&self, id: ProposalId, at: Timestamp) -> Option<(Timestamp, bool)> {
        self.late(at, self.watch.fwd.is_acked(id))
    }

    /// When own entry `id`, forwarded at `at`, is acted on: hedged when late, or sooner,
    /// with its target suspected, if the target stays silent.
    pub(super) fn hedge_at(&self, id: ProposalId, at: Timestamp) -> Option<Timestamp> {
        let (late, silent) = (self.lateness(id, at).map(|(t, _)| t), self.silent_at(at));
        late.into_iter().chain(silent).min()
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

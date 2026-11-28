//! Observer log following: pulling decided slots from voting members while outside the
//! latest epoch (#157).
//!
//! QuePaxa has no leader replication stream, so voting members never push decisions to
//! non-members. When [`Config::follow_interval`](crate::Config::follow_interval) is non-zero
//! and this replica is outside the latest epoch it knows, it starts by asking every active
//! voting member for `CatchUp` (the fastest responder becomes its preferred peer) and then
//! polls that single peer whenever `follow_interval` passes without word from a voting
//! member. If the preferred peer does not answer before the next tick, it rotates to the
//! next voting member. After several consecutive empty replies it probes another voting
//! member once so a preferred peer that is alive but partitioned from the voting majority
//! cannot stall the observer. Polling stops automatically once a membership change includes
//! this replica, or on fencing.

use alloc::vec::Vec;

use super::Replica;
use crate::codec::Body;
use crate::types::{NodeId, Slot, Timestamp};

/// Consecutive empty replies from the preferred peer before probing another voting member
/// (detects a live preferred peer that is partitioned or stalled from the voting majority).
const IDLE_PROBE_EVERY: u8 = 4;

/// State of the in-flight follow poll, if any.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
enum Poll {
    #[default]
    Idle,
    /// Waiting on the preferred peer (or the initial startup broadcast).
    Primary,
    /// Waiting on a periodic alternate-peer health probe; silence or an empty reply from
    /// that peer leaves the preferred peer intact, while actual progress switches to the
    /// responder.
    Alternate(NodeId),
}

/// Log-following state of an outside replica (observer or pre-join replica).
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct Follow {
    /// When to send the next follow `CatchUp` if no voting member speaks sooner.
    next_at: Option<Timestamp>,
    /// Voting member last seen answering promptly; kept while it keeps answering.
    peer: Option<NodeId>,
    /// Round-robin cursor when `peer` is absent, left the membership, or for alternate probes.
    cursor: usize,
    /// Whether a follow poll is awaiting an answer, and what kind.
    poll: Poll,
    /// Consecutive empty replies to `Poll::Primary` polls, capped at `IDLE_PROBE_EVERY`.
    idle_polls: u8,
}

impl Follow {
    /// Arms the follow timer and marks an initial probe as pending.
    pub(super) fn arm(&mut self, now: Timestamp, interval: u64) {
        self.next_at = Some(now.after(interval));
        self.poll = Poll::Primary;
        self.idle_polls = 0;
    }

    /// Arms the follow timer only if it is currently disarmed (e.g. after being demoted).
    pub(super) fn arm_if_idle(&mut self, now: Timestamp, interval: u64) {
        if self.next_at.is_none() {
            self.next_at = Some(now.after(interval));
            self.poll = Poll::Idle;
            self.idle_polls = 0;
        }
    }

    /// Postpones the next tick without marking a poll pending (a bulk catch-up is in flight).
    pub(super) fn postpone(&mut self, now: Timestamp, interval: u64) {
        if self.next_at.is_some() {
            self.next_at = Some(now.after(interval));
        }
    }

    /// Stops background polling (this replica became a voting member or fenced).
    pub(super) fn stop(&mut self) {
        self.next_at = None;
        self.poll = Poll::Idle;
        self.idle_polls = 0;
    }

    /// Locks onto `from` as the preferred follow peer (e.g. the fastest `ReadReply`).
    pub(super) fn prefer(&mut self, from: NodeId) {
        self.peer = Some(from);
        self.idle_polls = 0;
    }

    /// A voting member spoke: reset the idle timer, resolve any pending poll, and update the
    /// preferred peer when `from` delivered new progress (`advanced`) or answered first.
    pub(super) fn on_heard(&mut self, from: NodeId, advanced: bool, now: Timestamp, interval: u64) {
        if advanced {
            if self.poll != Poll::Idle || self.peer.is_none() {
                self.peer = Some(from);
            }
            self.poll = Poll::Idle;
            self.idle_polls = 0;
            self.next_at = Some(now.after(interval));
            return;
        }
        match self.poll {
            Poll::Primary if self.peer.is_none() || self.peer == Some(from) => {
                if self.peer.is_none() {
                    self.peer = Some(from);
                }
                self.poll = Poll::Idle;
                self.idle_polls = self.idle_polls.saturating_add(1).min(IDLE_PROBE_EVERY);
                self.next_at = Some(now.after(interval));
            }
            Poll::Alternate(target) if from == target => {
                // Alternate peer had nothing new either: keep the fastest preferred peer.
                self.poll = Poll::Idle;
                self.idle_polls = 0;
                self.next_at = Some(now.after(interval));
            }
            Poll::Idle => {
                if self.peer.is_none() {
                    self.peer = Some(from);
                }
                self.next_at = Some(now.after(interval));
            }
            Poll::Primary | Poll::Alternate(_) => {
                // Non-advancing message from an unpolled peer: leave the in-flight poll and its
                // timeout intact so a silent preferred peer still rotates and a stalled peer
                // cannot cancel an alternate health probe.
            }
        }
    }

    /// Next poll deadline while outside with a non-zero interval.
    pub(super) fn deadline(&self, outside: bool, interval: u64) -> Option<Timestamp> {
        (outside && interval > 0).then_some(self.next_at).flatten()
    }

    /// If due, schedules the next tick and returns the voting member to poll.
    pub(super) fn fire(
        &mut self,
        now: Timestamp,
        interval: u64,
        outside: bool,
        peers: &[NodeId],
    ) -> Option<NodeId> {
        if !outside || interval == 0 {
            self.stop();
            return None;
        }
        if !self.next_at.is_some_and(|t| t <= now) {
            return None;
        }
        self.next_at = Some(now.after(interval));
        if peers.is_empty() {
            self.peer = None;
            self.poll = Poll::Idle;
            self.idle_polls = 0;
            return None;
        }
        match (self.peer, self.poll) {
            (Some(p), Poll::Idle) if peers.contains(&p) => {
                if self.idle_polls >= IDLE_PROBE_EVERY && peers.len() > 1 {
                    let mut idx = self.cursor % peers.len();
                    if peers[idx] == p {
                        idx = (idx + 1) % peers.len();
                    }
                    self.cursor = (idx + 1) % peers.len();
                    let target = peers[idx];
                    self.poll = Poll::Alternate(target);
                    self.idle_polls = 0;
                    return Some(target);
                }
                self.poll = Poll::Primary;
                Some(p)
            }
            (Some(p), Poll::Alternate(_)) if peers.contains(&p) => {
                // Alternate probe timed out; fall back to the preferred peer.
                self.poll = Poll::Primary;
                self.idle_polls = 0;
                Some(p)
            }
            (Some(prev), _) => {
                let next = peers
                    .iter()
                    .position(|&m| m == prev)
                    .map_or(self.cursor % peers.len(), |i| (i + 1) % peers.len());
                self.cursor = (next + 1) % peers.len();
                let target = peers[next];
                self.peer = Some(target);
                self.poll = Poll::Primary;
                self.idle_polls = 0;
                Some(target)
            }
            (None, _) => {
                let next = self.cursor % peers.len();
                self.cursor = (next + 1) % peers.len();
                let target = peers[next];
                self.peer = Some(target);
                self.poll = Poll::Primary;
                self.idle_polls = 0;
                Some(target)
            }
        }
    }
}

impl Replica {
    /// Active or pending voting members from `committed` onward, excluding `self.me`.
    /// Expired members from earlier unpruned epochs are excluded so the observer never
    /// stays pinned to a removed member after a reconfiguration takes effect.
    fn follow_peers(&self) -> Vec<NodeId> {
        let mut peers: Vec<NodeId> = self
            .schedule
            .member_sets_from(Slot(self.committed))
            .flatten()
            .copied()
            .filter(|&m| m != self.me)
            .collect();
        peers.sort_unstable();
        peers.dedup();
        peers
    }

    /// Whether `node` is a voting member in any active or pending epoch from `committed` on.
    pub(super) fn is_follow_peer(&self, node: NodeId) -> bool {
        self.schedule.member_sets_from(Slot(self.committed)).any(|s| s.contains(&node))
    }

    /// Starts following the log on startup when configured and outside the latest epoch.
    /// After a restart, `probe_peers` already sent the initial `CatchUp` to every peer.
    pub(super) fn start_follow(&mut self, already_probed: bool) {
        let interval = self.cfg.follow_interval;
        if interval == 0 || !self.outside() {
            return;
        }
        let peers = self.follow_peers();
        if !already_probed {
            for peer in peers {
                self.send(peer, Body::CatchUp { from: Slot(self.committed) });
            }
        }
        self.follow.arm(self.now, interval);
    }

    /// Notes a message from an active voting member (or non-empty `Entries` from a newer
    /// unknown member) so the observer stays locked onto the fastest responder and resets
    /// its idle poll timer.
    pub(super) fn note_follow_heard(&mut self, from: NodeId, is_entries: bool, advanced: bool) {
        let interval = self.cfg.follow_interval;
        if interval == 0 || !self.outside() {
            self.follow.stop();
            return;
        }
        if (is_entries && advanced) || self.is_follow_peer(from) {
            self.follow.on_heard(from, advanced, self.now, interval);
        }
    }

    /// Sends one periodic `CatchUp` when idle outside the membership; does not set
    /// `catch_up.request`, so an empty reply never throttles a read-triggered catch-up.
    pub(super) fn fire_follow(&mut self) {
        let outside = self.outside();
        let interval = self.cfg.follow_interval;
        if !outside || interval == 0 {
            self.follow.stop();
            return;
        }
        if !self.follow.deadline(outside, interval).is_some_and(|t| t <= self.now) {
            return;
        }
        if !self.catch_up_due() {
            self.follow.postpone(self.now, interval);
            return;
        }
        let peers = self.follow_peers();
        if let Some(peer) = self.follow.fire(self.now, interval, outside, &peers) {
            self.send(peer, Body::CatchUp { from: Slot(self.committed) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PEERS: [NodeId; 3] = [NodeId(0), NodeId(1), NodeId(2)];

    #[test]
    fn first_responder_becomes_preferred_and_stays_while_answering() {
        let mut f = Follow::default();
        f.arm(Timestamp(0), 50);
        assert_eq!(f.deadline(true, 50), Some(Timestamp(50)));
        // Peer 1 answers first; slower peer 2 answering later does not displace it.
        f.on_heard(NodeId(1), false, Timestamp(10), 50);
        f.on_heard(NodeId(2), false, Timestamp(20), 50);
        assert_eq!(f.deadline(true, 50), Some(Timestamp(70)));
        assert_eq!(f.fire(Timestamp(70), 50, true, &PEERS), Some(NodeId(1)));
        f.on_heard(NodeId(1), true, Timestamp(80), 50);
        assert_eq!(f.fire(Timestamp(130), 50, true, &PEERS), Some(NodeId(1)));
    }

    #[test]
    fn unresponsive_peer_rotates_on_next_tick() {
        let mut f = Follow::default();
        f.arm(Timestamp(0), 50);
        f.on_heard(NodeId(0), true, Timestamp(10), 50);
        assert_eq!(f.fire(Timestamp(60), 50, true, &PEERS), Some(NodeId(0)));
        // Peer 0 never answered before Timestamp(110): rotate to peer 1, then peer 2.
        assert_eq!(f.fire(Timestamp(110), 50, true, &PEERS), Some(NodeId(1)));
        assert_eq!(f.fire(Timestamp(160), 50, true, &PEERS), Some(NodeId(2)));
        // Peer 2 answers: lock onto peer 2.
        f.on_heard(NodeId(2), true, Timestamp(170), 50);
        assert_eq!(f.fire(Timestamp(220), 50, true, &PEERS), Some(NodeId(2)));
    }

    #[test]
    fn consecutive_empty_polls_probe_alternate_and_switch_only_on_progress() {
        let mut f = Follow::default();
        f.arm(Timestamp(0), 50);
        f.on_heard(NodeId(0), true, Timestamp(10), 50);
        let mut now = Timestamp(60);
        for _ in 0..IDLE_PROBE_EVERY {
            assert_eq!(f.fire(now, 50, true, &PEERS), Some(NodeId(0)));
            now = now.after(10);
            f.on_heard(NodeId(0), false, now, 50);
            now = now.after(50);
        }
        // Next tick probes another peer without discarding peer 0 yet.
        let alt = f.fire(now, 50, true, &PEERS).unwrap();
        assert_ne!(alt, NodeId(0));
        // Empty reply from alternate keeps preferred peer 0.
        now = now.after(10);
        f.on_heard(alt, false, now, 50);
        now = now.after(50);
        assert_eq!(f.fire(now, 50, true, &PEERS), Some(NodeId(0)));
        // After another IDLE_PROBE_EVERY empty polls, alternate returning progress takes over.
        for _ in 0..IDLE_PROBE_EVERY {
            now = now.after(10);
            f.on_heard(NodeId(0), false, now, 50);
            now = now.after(50);
            let _ = f.fire(now, 50, true, &PEERS);
        }
        // The last fire in the loop already sent the alternate probe.
        now = now.after(10);
        f.on_heard(NodeId(2), true, now, 50);
        now = now.after(50);
        assert_eq!(f.fire(now, 50, true, &PEERS), Some(NodeId(2)));
    }

    #[test]
    fn peer_removed_by_reconfiguration_is_replaced() {
        let mut f = Follow::default();
        f.arm(Timestamp(0), 50);
        f.on_heard(NodeId(0), true, Timestamp(10), 50);
        let new_peers = [NodeId(1), NodeId(2), NodeId(4)];
        let target = f.fire(Timestamp(60), 50, true, &new_peers).unwrap();
        assert!(new_peers.contains(&target));
    }

    #[test]
    fn becoming_a_member_disarms_follow() {
        let mut f = Follow::default();
        f.arm(Timestamp(0), 50);
        assert_eq!(f.deadline(false, 50), None);
        assert_eq!(f.fire(Timestamp(50), 50, false, &PEERS), None);
        assert_eq!(f.deadline(true, 50), None, "stopped once no longer outside");
    }

    #[test]
    fn unrelated_empty_message_does_not_cancel_alternate_probe() {
        let mut f = Follow::default();
        f.arm(Timestamp(0), 50);
        f.on_heard(NodeId(0), true, Timestamp(10), 50);
        let mut now = Timestamp(60);
        for _ in 0..IDLE_PROBE_EVERY {
            assert_eq!(f.fire(now, 50, true, &PEERS), Some(NodeId(0)));
            now = now.after(10);
            f.on_heard(NodeId(0), false, now, 50);
            now = now.after(50);
        }
        let alt = f.fire(now, 50, true, &PEERS).unwrap();
        assert_ne!(alt, NodeId(0));
        // Stalled preferred peer 0 sends a non-advancing message before `alt` responds.
        now = now.after(5);
        f.on_heard(NodeId(0), false, now, 50);
        // `alt` responds with new progress and must still take over as preferred peer.
        now = now.after(5);
        f.on_heard(alt, true, now, 50);
        now = now.after(50);
        assert_eq!(f.fire(now, 50, true, &PEERS), Some(alt));
    }

    #[test]
    fn unrelated_empty_message_does_not_prevent_rotating_off_silent_peer() {
        let mut f = Follow::default();
        f.arm(Timestamp(0), 50);
        f.on_heard(NodeId(0), true, Timestamp(10), 50);
        assert_eq!(f.fire(Timestamp(60), 50, true, &PEERS), Some(NodeId(0)));
        // Peer 1 speaks without progress while preferred peer 0 stays silent.
        f.on_heard(NodeId(1), false, Timestamp(80), 50);
        // Primary poll deadline stays at 110 and rotates off silent peer 0.
        assert_eq!(f.deadline(true, 50), Some(Timestamp(110)));
        assert_eq!(f.fire(Timestamp(110), 50, true, &PEERS), Some(NodeId(1)));
    }
}

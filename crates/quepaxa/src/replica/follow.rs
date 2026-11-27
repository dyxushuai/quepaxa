//! Observer log following: pulling decided slots from voting members while outside the
//! latest epoch (#157).
//!
//! QuePaxa has no leader replication stream, so voting members never push decisions to
//! non-members. When [`Config::follow_interval`](crate::Config::follow_interval) is non-zero
//! and this replica is outside the latest epoch it knows, it starts by asking every known
//! voting member for `CatchUp` (the fastest responder becomes its preferred peer) and then
//! polls that single peer whenever `follow_interval` passes without word from a voting
//! member. If the preferred peer does not answer before the next tick, it rotates to the
//! next voting member. Polling stops automatically once a membership change includes this
//! replica, or on fencing.

use alloc::vec::Vec;

use super::Replica;
use crate::codec::Body;
use crate::types::{NodeId, Slot, Timestamp};

/// Log-following state of an outside replica (observer or pre-join replica).
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct Follow {
    /// When to send the next follow `CatchUp` if no voting member speaks sooner.
    next_at: Option<Timestamp>,
    /// Voting member last seen answering promptly; kept while it keeps answering.
    peer: Option<NodeId>,
    /// Fallback round-robin cursor when `peer` is absent or left the membership.
    cursor: usize,
    /// A follow poll was sent and has not been answered by any voting member yet.
    pending: bool,
}

impl Follow {
    /// Arms the follow timer and marks an initial probe as pending.
    pub(super) fn arm(&mut self, now: Timestamp, interval: u64) {
        self.next_at = Some(now.after(interval));
        self.pending = true;
    }

    /// Arms the follow timer only if it is currently disarmed (e.g. after being demoted).
    pub(super) fn arm_if_idle(&mut self, now: Timestamp, interval: u64) {
        if self.next_at.is_none() {
            self.next_at = Some(now.after(interval));
            self.pending = false;
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
        self.pending = false;
    }

    /// Locks onto `from` as the preferred follow peer (e.g. the fastest `ReadReply`).
    pub(super) fn prefer(&mut self, from: NodeId) {
        self.peer = Some(from);
    }

    /// A voting member spoke: reset the idle timer, clear `pending`, and remember `from` if
    /// it was the first to answer a pending probe or no peer is chosen yet.
    pub(super) fn on_heard(&mut self, from: NodeId, now: Timestamp, interval: u64) {
        if self.pending || self.peer.is_none() {
            self.peer = Some(from);
        }
        self.pending = false;
        self.next_at = Some(now.after(interval));
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
            self.pending = false;
            return None;
        }
        let target = match self.peer {
            Some(p) if !self.pending && peers.contains(&p) => p,
            Some(prev) => {
                let next = peers
                    .iter()
                    .position(|&m| m == prev)
                    .map_or(self.cursor % peers.len(), |i| (i + 1) % peers.len());
                self.cursor = (next + 1) % peers.len();
                peers[next]
            }
            None => {
                let next = self.cursor % peers.len();
                self.cursor = (next + 1) % peers.len();
                peers[next]
            }
        };
        self.peer = Some(target);
        self.pending = true;
        Some(target)
    }
}

impl Replica {
    /// Voting members this replica can pull decided slots from.
    fn follow_peers(&self) -> Vec<NodeId> {
        self.schedule.everyone().into_iter().filter(|&m| m != self.me).collect()
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

    /// Notes a message from a voting member (or `Entries` from an unknown newer member) so
    /// the observer stays locked onto the fastest responder and resets its idle poll timer.
    pub(super) fn note_follow_heard(&mut self, from: NodeId, is_entries: bool) {
        let interval = self.cfg.follow_interval;
        if interval == 0 || !self.outside() {
            self.follow.stop();
            return;
        }
        if is_entries || self.schedule.knows(from) {
            self.follow.on_heard(from, self.now, interval);
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
        f.on_heard(NodeId(1), Timestamp(10), 50);
        f.on_heard(NodeId(2), Timestamp(20), 50);
        assert_eq!(f.deadline(true, 50), Some(Timestamp(70)));
        assert_eq!(f.fire(Timestamp(70), 50, true, &PEERS), Some(NodeId(1)));
        f.on_heard(NodeId(1), Timestamp(80), 50);
        assert_eq!(f.fire(Timestamp(130), 50, true, &PEERS), Some(NodeId(1)));
    }

    #[test]
    fn unresponsive_peer_rotates_on_next_tick() {
        let mut f = Follow::default();
        f.arm(Timestamp(0), 50);
        f.on_heard(NodeId(0), Timestamp(10), 50);
        assert_eq!(f.fire(Timestamp(60), 50, true, &PEERS), Some(NodeId(0)));
        // Peer 0 never answered before Timestamp(110): rotate to peer 1, then peer 2.
        assert_eq!(f.fire(Timestamp(110), 50, true, &PEERS), Some(NodeId(1)));
        assert_eq!(f.fire(Timestamp(160), 50, true, &PEERS), Some(NodeId(2)));
        // Peer 2 answers: lock onto peer 2.
        f.on_heard(NodeId(2), Timestamp(170), 50);
        assert_eq!(f.fire(Timestamp(220), 50, true, &PEERS), Some(NodeId(2)));
    }

    #[test]
    fn peer_removed_by_reconfiguration_is_replaced() {
        let mut f = Follow::default();
        f.arm(Timestamp(0), 50);
        f.on_heard(NodeId(0), Timestamp(10), 50);
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
}

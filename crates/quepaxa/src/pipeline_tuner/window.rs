//! Fixed-memory sample windows and follower commit-lag tracking for [`super::PipelineTuner`].

use alloc::collections::BTreeMap;

use crate::types::{NodeId, Slot, Timestamp};

const DECIDED_RING: usize = 64;
const PEER_SAMPLES: usize = 16;
const PEER_PRUNE_NS: u64 = 60_000_000_000;

/// Fixed-size stride-2 decimated sample window for deterministic `O(1)` p90 estimation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct TailWindow<const N: usize> {
    samples: [u64; N],
    len: usize,
    stride: u32,
    seen: u32,
}

impl<const N: usize> TailWindow<N> {
    pub(super) const fn new() -> Self {
        Self { samples: [0; N], len: 0, stride: 1, seen: 0 }
    }

    pub(super) fn reset(&mut self) {
        self.len = 0;
        self.stride = 1;
        self.seen = 0;
    }

    pub(super) fn push(&mut self, ns: u64) {
        self.seen = self.seen.saturating_add(1);
        if !self.seen.is_multiple_of(self.stride) {
            return;
        }
        if self.len == N {
            for i in 0..(N / 2) {
                self.samples[i] = self.samples[i * 2];
            }
            self.len = N / 2;
            self.stride = self.stride.saturating_mul(2);
        }
        self.samples[self.len] = ns;
        self.len += 1;
    }

    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(super) fn p90(&mut self) -> u64 {
        if self.len == 0 {
            return 0;
        }
        let slice = &mut self.samples[..self.len];
        slice.sort_unstable();
        slice[((self.len - 1) * 9) / 10]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
struct PeerLag {
    last_heard: Timestamp,
    last_committed: u64,
    /// Advance-triggered `now - decided_at[commit_index - 1]` samples (invariant to `W`).
    lag: TailWindow<PEER_SAMPLES>,
    /// Stall lower bound (`now - decided_at[commit_index]`) while `commit_index` is stuck.
    max_stall_ns: u64,
    high_streak: u8,
}

impl PeerLag {
    const fn new(now: Timestamp, committed: u64) -> Self {
        Self {
            last_heard: now,
            last_committed: committed,
            lag: TailWindow::new(),
            max_stall_ns: 0,
            high_streak: 0,
        }
    }
}

/// Envelope `commit_index` observation from a live peer.
pub(super) struct PeerReport {
    pub(super) peer: NodeId,
    pub(super) committed: u64,
    pub(super) local_committed: u64,
    pub(super) now: Timestamp,
    pub(super) silence_limit_ns: u64,
}

/// Tracks live follower commit lag from envelope `commit_index` headers against a bounded
/// ring of `(slot, decided_at)` timestamps.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct PeerTracker {
    decided_ring: [(u64, Timestamp); DECIDED_RING],
    decided_valid: u64,
    first_recorded: Option<u64>,
    peers: BTreeMap<NodeId, PeerLag>,
}

impl PeerTracker {
    pub(super) fn new() -> Self {
        Self {
            decided_ring: [(0, Timestamp(0)); DECIDED_RING],
            decided_valid: 0,
            first_recorded: None,
            peers: BTreeMap::new(),
        }
    }

    pub(super) fn clear_peers(&mut self) {
        self.peers.clear();
    }

    pub(super) fn note_decided(&mut self, slot: Slot, now: Timestamp) {
        let idx = (slot.0 as usize) & (DECIDED_RING - 1);
        self.decided_ring[idx] = (slot.0, now);
        self.decided_valid = self.decided_valid.saturating_add(1).min(DECIDED_RING as u64);
        if self.first_recorded.is_none() {
            self.first_recorded = Some(slot.0);
        }
    }

    /// Feeds one live peer's advertised `commit_index`. When `peer` has been continuously
    /// active, `!draining`, and `abort_limit` is `Some(limit)`, updates and returns the peer's
    /// consecutive sample streak above `limit`.
    pub(super) fn observe(
        &mut self,
        report: PeerReport,
        draining: bool,
        abort_limit: Option<u64>,
    ) -> Option<u8> {
        let PeerReport { peer, committed: peer_committed, local_committed, now, silence_limit_ns } = report;
        let Some(first) = self.first_recorded else {
            self.peers
                .entry(peer)
                .and_modify(|p| {
                    p.last_heard = now;
                    p.last_committed = p.last_committed.max(peer_committed);
                })
                .or_insert_with(|| PeerLag::new(now, peer_committed));
            return None;
        };
        let (continuous, advanced) = match self.peers.get_mut(&peer) {
            Some(p) => {
                let gap = now.0.saturating_sub(p.last_heard.0);
                p.last_heard = now;
                if gap > silence_limit_ns {
                    p.last_committed = peer_committed;
                    p.max_stall_ns = 0;
                    p.high_streak = 0;
                    (false, false)
                } else if peer_committed > p.last_committed {
                    p.last_committed = peer_committed;
                    (true, true)
                } else {
                    (true, false)
                }
            }
            None => {
                self.peers.insert(peer, PeerLag::new(now, peer_committed));
                (false, false)
            }
        };
        if !continuous || draining {
            return None;
        }
        let lag_ns = if advanced {
            let slot = peer_committed.saturating_sub(1);
            let sample = self.lookup_slot_age(slot, local_committed, first, now)?;
            let p = self.peers.get_mut(&peer)?;
            p.lag.push(sample);
            p.max_stall_ns = 0;
            sample
        } else if peer_committed < local_committed {
            let stall = self.lookup_slot_age(peer_committed, local_committed, first, now)?;
            let p = self.peers.get_mut(&peer)?;
            p.max_stall_ns = p.max_stall_ns.max(stall);
            stall
        } else {
            return None;
        };

        let limit = abort_limit?;
        let p = self.peers.get_mut(&peer)?;
        if lag_ns > limit {
            p.high_streak = p.high_streak.saturating_add(1);
        } else {
            p.high_streak = 0;
        }
        Some(p.high_streak)
    }

    pub(super) fn max_lag_ns(&mut self) -> u64 {
        let mut worst = 0u64;
        for p in self.peers.values_mut() {
            if !p.lag.is_empty() || p.max_stall_ns > 0 {
                worst = worst.max(p.lag.p90().max(p.max_stall_ns));
            }
        }
        worst
    }

    pub(super) fn prune(&mut self, now: Timestamp) {
        self.peers.retain(|_, p| now.0.saturating_sub(p.last_heard.0) <= PEER_PRUNE_NS);
    }

    pub(super) fn reset_window(&mut self) {
        for p in self.peers.values_mut() {
            p.lag.reset();
            p.max_stall_ns = 0;
            p.high_streak = 0;
        }
    }

    fn lookup_slot_age(
        &self,
        target_slot: u64,
        local_committed: u64,
        first_recorded: u64,
        now: Timestamp,
    ) -> Option<u64> {
        if target_slot >= local_committed {
            return Some(0);
        }
        if target_slot < first_recorded {
            return None;
        }
        let behind = local_committed - target_slot;
        if behind <= DECIDED_RING as u64 {
            let (slot, ts) = self.decided_ring[(target_slot as usize) & (DECIDED_RING - 1)];
            if slot == target_slot {
                return Some(now.0.saturating_sub(ts.0));
            }
        }
        if self.decided_valid == DECIDED_RING as u64 {
            let oldest_slot = local_committed.saturating_sub(DECIDED_RING as u64);
            let (slot, ts) = self.decided_ring[(oldest_slot as usize) & (DECIDED_RING - 1)];
            if slot == oldest_slot {
                return Some(now.0.saturating_sub(ts.0));
            }
        }
        None
    }
}

pub(super) fn smooth_u64(prev: u64, sample: u64) -> u64 {
    if prev == 0 { sample } else { prev - prev / 4 + sample / 4 }
}

pub(super) fn smooth_u128(prev: u128, sample: u128) -> u128 {
    if prev == 0 { sample } else { prev - prev / 4 + sample / 4 }
}

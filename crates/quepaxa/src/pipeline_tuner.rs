//! Online concurrency (`in_flight_slots`) tuner (#175).
//!
//! Adjusts the leader's active in-flight slot limit `W` over a power-of-two ladder within
//! `1..=max_w` (where `max_w <= LAG = 64`), without touching safety (FR1–FR4) or memory
//! budgets (FR7, decoupled in #251).
//!
//! Three mechanisms work together in an explicit state machine (`Warmup`, `Exploit`,
//! `DrainProbe`, `Probe`):
//!
//! 1. **Rate-limited saturation guard (`on_saturated`)**: when `entries.cut` hits
//!    `max_batch_bytes` with leftover entries and the active slot window is full, step `W`
//!    up one rung at most once per `50 ms` so a congested WAN ramps `1→2→4→8` in ~150 ms
//!    while a single LAN disk stall never leaps straight to `max_w` inside one `drive()` call.
//! 2. **Smoothed demand floor (`w_floor`)**: each window estimates `bytes_per_rtt` using a
//!    smoothed RTT against `3/8` of `max_batch_bytes`. `W` never probes below `w_floor`, and
//!    two consecutive windows above `current` raise `W` to `w_floor`.
//! 3. **Cluster-latency explore/exploit + distress backoff**: probes adjacent rungs scored by
//!    `p90(leader_slot_residence)`, guarded symmetrically by live follower lag (`now -
//!    decided_at[commit_index - 1]` on commit advancement, invariant to `W` and slot rate,
//!    plus a stall lower bound). Upward probes require at least `max(8%, 10 ms)` gain with no
//!    distress or follower-lag regression; sustained distress while `W > w_floor` steps `W`
//!    down directly so high-`W` overload never traps the leader.

mod window;

use self::window::{PeerReport, PeerTracker, TailWindow, smooth_u64, smooth_u128};
use crate::types::{NodeId, Slot, Timestamp};

const MAX_LADDER: usize = 8;
const MAX_SAMPLES: usize = 64;
const MIN_SLOTS: u32 = 8;
const MIN_LOW_RATE_SLOTS: u32 = 4;
const MIN_PROBE_NS: u64 = 120_000_000;
const MAX_PROBE_NS: u64 = 500_000_000;
const MIN_EXPLOIT_NS: u64 = 400_000_000;
const MAX_EXPLOIT_NS: u64 = 800_000_000;
const MAX_WINDOW_NS: u64 = 2_000_000_000;
const MAX_BACKOFF: u32 = 32;
const SATURATION_HOLD_WINDOWS: u32 = 6;
/// Minimum interval between consecutive `on_saturated` rung jumps (50 ms).
const SAT_COOLDOWN_NS: u64 = 50_000_000;
/// Consecutive windows with `w_floor > current` required before forcing a floor jump.
const FLOOR_JUMP_STREAK: u8 = 2;
/// Consecutive distressed windows at `W > w_floor` required before stepping `W` down.
const DISTRESS_BACKOFF_STREAK: u8 = 2;
/// Clean windows required after a distress step-down before `on_saturated` may step `W` up again.
const DISTRESS_HOLD_WINDOWS: u32 = 2;
const EARLY_ABORT_LAG_SLACK_NS: u64 = 60_000_000;
const PROBE_LAG_SLACK_NS: u64 = 40_000_000;
/// Minimum absolute leader-residence gain required for an upward latency probe (10 ms),
/// preventing LAN clusters from trading 1–2 ms batch-wait jitter for 4x–8x fsync churn.
const UP_GAIN_SLACK_NS: u64 = 10_000_000;
const DOWN_TOL_SLACK_NS: u64 = 5_000_000;
const REGIME_SCORE_SLACK_NS: u64 = 10_000_000;
const REGIME_BPR_SLACK_BYTES: u128 = 32 * 1024;
const REGIME_SHIFT_STREAK: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
enum Mode {
    Warmup,
    Exploit,
    DrainProbe { origin: u32, origin_score: u64, origin_lag: u64 },
    Probe { origin: u32, origin_score: u64, origin_lag: u64 },
}

/// Deterministic, `no_std`, bounded-memory online tuner for `in_flight_slots`.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(crate) struct PipelineTuner {
    ladder: [u32; MAX_LADDER],
    ladder_len: usize,
    max_w: u32,
    max_batch_bytes: u32,
    current: u32,
    mode: Mode,
    drain_left: u32,

    baseline_score: u64,
    baseline_lag: u64,
    baseline_bpr: u128,
    /// Propagation RTT baseline (never inflated by distressed/congested windows) used for
    /// `w_floor` so queuing delay under overload cannot lock `W` at high rungs.
    min_rtt: u64,

    up_backoff: u32,
    down_backoff: u32,
    up_wait: u32,
    down_wait: u32,
    saturation_hold: u32,
    distress_hold: u32,
    last_sat_step: Option<Timestamp>,
    shift_streak: u8,
    floor_streak: u8,
    distress_streak: u8,

    win_start: Timestamp,
    win_slots: u32,
    win_bytes: u64,
    win_distress: bool,
    high_res_streak: u8,
    residences: TailWindow<MAX_SAMPLES>,
    peers: PeerTracker,
}

impl PipelineTuner {
    /// Creates a tuner over `1..=max_w`, starting at `1` so low-load and LAN workloads
    /// pay no warmup penalty while saturated workloads ramp via `on_saturated`.
    pub(crate) fn new(max_w: u32, max_batch_bytes: u32, now: Timestamp) -> Self {
        let max_w = max_w.max(1);
        let (ladder, ladder_len) = build_ladder(max_w);
        Self {
            ladder,
            ladder_len,
            max_w,
            max_batch_bytes: max_batch_bytes.max(64),
            current: 1,
            mode: Mode::Warmup,
            drain_left: 0,
            baseline_score: 0,
            baseline_lag: 0,
            baseline_bpr: 0,
            min_rtt: 0,
            up_backoff: 1,
            down_backoff: 1,
            up_wait: 0,
            down_wait: 0,
            saturation_hold: 0,
            distress_hold: 0,
            last_sat_step: None,
            shift_streak: 0,
            floor_streak: 0,
            distress_streak: 0,
            win_start: now,
            win_slots: 0,
            win_bytes: 0,
            win_distress: false,
            high_res_streak: 0,
            residences: TailWindow::new(),
            peers: PeerTracker::new(),
        }
    }

    pub(crate) fn current(&self) -> u32 {
        self.current
    }

    /// Resets tuning state (e.g., on leadership loss or explicit `set_in_flight_slots`),
    /// anchoring at `w` clamped to `1..=max_w`.
    pub(crate) fn reset(&mut self, w: u32, now: Timestamp) {
        self.current = w.clamp(1, self.max_w);
        self.mode = Mode::Warmup;
        self.drain_left = 0;
        self.baseline_score = 0;
        self.baseline_lag = 0;
        self.baseline_bpr = 0;
        self.min_rtt = 0;
        self.up_backoff = 1;
        self.down_backoff = 1;
        self.up_wait = 0;
        self.down_wait = 0;
        self.saturation_hold = 0;
        self.distress_hold = 0;
        self.last_sat_step = None;
        self.shift_streak = 0;
        self.floor_streak = 0;
        self.distress_streak = 0;
        self.peers.clear_peers();
        self.reset_window(now);
    }

    /// Records when `slot` entered the contiguous decided prefix on this replica.
    pub(crate) fn note_decided(&mut self, slot: Slot, now: Timestamp) {
        self.peers.note_decided(slot, now);
    }

    /// Called when `entries.cut` hit `max_batch_bytes` with leftover entries while the active
    /// slot window was full. Steps up at most one rung per `SAT_COOLDOWN_NS` (50 ms), never
    /// while distress is active or cooling down after a distress backoff.
    pub(crate) fn on_saturated(&mut self, now: Timestamp) -> Option<u32> {
        if self.win_distress || self.distress_streak > 0 || self.distress_hold > 0 {
            return None;
        }
        self.saturation_hold = SATURATION_HOLD_WINDOWS;
        if let Some(prev) = self.last_sat_step
            && now.0.saturating_sub(prev.0) < SAT_COOLDOWN_NS
        {
            return None;
        }
        let next = self.next_rung(self.current);
        if next == self.current {
            return None;
        }
        self.last_sat_step = Some(now);
        self.jump_up(next, now);
        Some(next)
    }

    /// Records a distress signal (retransmit or lost leader slot) in the current window.
    /// Immediately aborts an upward probe so contention never stretches tail latency.
    pub(crate) fn on_distress(&mut self, now: Timestamp) -> Option<u32> {
        self.win_distress = true;
        if let Mode::Probe { origin, .. } = self.mode
            && self.current > origin
        {
            return Some(self.abort_upward_probe(origin, now));
        }
        None
    }

    /// Feeds a live peer's advertised `commit_index` from a non-recovery message envelope.
    pub(crate) fn on_peer_commit(
        &mut self,
        peer: NodeId,
        peer_committed: u64,
        local_committed: u64,
        now: Timestamp,
        silence_limit_ns: u64,
    ) -> Option<u32> {
        let upward_probe = match self.mode {
            Mode::Probe { origin, origin_lag, .. } if self.current > origin => {
                Some((origin, origin_lag.saturating_add((origin_lag / 2).max(EARLY_ABORT_LAG_SLACK_NS))))
            }
            _ => None,
        };
        let streak = self.peers.observe(
            PeerReport { peer, committed: peer_committed, local_committed, now, silence_limit_ns },
            self.drain_left > 0,
            upward_probe.map(|(_, limit)| limit),
        )?;
        if let Some((origin, _)) = upward_probe
            && streak >= 3
        {
            return Some(self.abort_upward_probe(origin, now));
        }
        None
    }

    /// Records one non-hedged leader-committed slot. Returns `Some(new_w)` on change.
    pub(crate) fn on_slot_committed(
        &mut self,
        weight: u32,
        residence_ns: u64,
        slot_rtt_ns: u64,
        now: Timestamp,
    ) -> Option<u32> {
        if self.ladder_len <= 1 {
            return None;
        }
        if self.drain_left > 0 {
            self.drain_left -= 1;
            if self.drain_left == 0 {
                if let Mode::DrainProbe { origin, origin_score, origin_lag } = self.mode {
                    self.mode = Mode::Probe { origin, origin_score, origin_lag };
                }
                self.reset_window(now);
            }
            return None;
        }

        if self.win_slots == 0 && now.0.saturating_sub(self.win_start.0) > MAX_WINDOW_NS {
            self.win_start = now;
        }
        self.residences.push(residence_ns);
        self.win_bytes = self.win_bytes.saturating_add(u64::from(weight));
        self.win_slots = self.win_slots.saturating_add(1);

        if let Mode::Probe { origin, origin_score, .. } = self.mode
            && self.current > origin
            && self.win_slots >= 4
        {
            let res_limit = origin_score.saturating_mul(3) / 2 + PROBE_LAG_SLACK_NS;
            if residence_ns > res_limit {
                self.high_res_streak = self.high_res_streak.saturating_add(1);
                if self.high_res_streak >= 3 {
                    return Some(self.abort_upward_probe(origin, now));
                }
            } else {
                self.high_res_streak = 0;
            }
        }

        let elapsed = now.0.saturating_sub(self.win_start.0);
        let min_dur = match self.mode {
            Mode::Probe { .. } => slot_rtt_ns.saturating_mul(4).clamp(MIN_PROBE_NS, MAX_PROBE_NS),
            Mode::Warmup | Mode::Exploit | Mode::DrainProbe { .. } => {
                slot_rtt_ns.saturating_mul(6).clamp(MIN_EXPLOIT_NS, MAX_EXPLOIT_NS)
            }
        };
        let ready = (self.win_slots >= MIN_SLOTS && elapsed >= min_dur)
            || (self.win_slots >= MIN_LOW_RATE_SLOTS && elapsed >= MAX_WINDOW_NS);
        if !ready {
            return None;
        }
        self.finish_window(elapsed, slot_rtt_ns, now)
    }

    fn finish_window(&mut self, elapsed: u64, slot_rtt_ns: u64, now: Timestamp) -> Option<u32> {
        let score = self.residences.p90();
        let peer_lag = self.peers.max_lag_ns();
        let distress = self.win_distress;
        let raw_rtt = if slot_rtt_ns > 0 { slot_rtt_ns } else { score.max(1) };
        if self.min_rtt == 0 || raw_rtt < self.min_rtt {
            self.min_rtt = raw_rtt.max(1);
        } else if !distress {
            // Drift upward very slowly only on clean windows so genuine route changes track
            // while overload RTT spikes cannot inflate `w_floor`.
            self.min_rtt = self.min_rtt.saturating_add((raw_rtt - self.min_rtt) / 32).max(1);
        }
        let bpr = (u128::from(self.win_bytes).saturating_mul(u128::from(self.min_rtt)))
            / u128::from(elapsed.max(1));
        let target_fill = ((u128::from(self.max_batch_bytes) * 3) / 8).max(64);
        let needed = bpr.div_ceil(target_fill).max(1).min(u128::from(self.max_w)) as u32;
        let w_floor = self.ceil_rung(needed);

        self.peers.prune(now);

        self.distress_streak = if distress { self.distress_streak.saturating_add(1) } else { 0 };
        if !distress {
            self.distress_hold = self.distress_hold.saturating_sub(1);
        }

        // Sustained retransmits or lost leader slots above the propagation-RTT demand floor
        // signal pipeline overload: step down one rung directly instead of getting trapped at
        // high `W`.
        if matches!(self.mode, Mode::Warmup | Mode::Exploit)
            && self.distress_streak >= DISTRESS_BACKOFF_STREAK
            && self.current > w_floor
        {
            let old = self.current;
            let prev = self.prev_rung(old).max(w_floor);
            self.current = prev;
            self.mode = Mode::Warmup;
            self.drain_left = old;
            self.saturation_hold = 0;
            self.distress_hold = DISTRESS_HOLD_WINDOWS;
            self.distress_streak = 0;
            self.floor_streak = 0;
            self.up_backoff = self.up_backoff.saturating_mul(2).clamp(4, MAX_BACKOFF);
            self.up_wait = 0;
            self.down_wait = 0;
            self.shift_streak = 0;
            self.reset_window(now);
            return Some(prev);
        }

        if !distress && w_floor > self.current {
            self.floor_streak = self.floor_streak.saturating_add(1);
            if self.floor_streak >= FLOOR_JUMP_STREAK {
                self.jump_up(w_floor, now);
                return Some(w_floor);
            }
        } else {
            self.floor_streak = 0;
        }

        let changed = match self.mode {
            Mode::Warmup => {
                self.enter_exploit(score, peer_lag, bpr);
                None
            }
            Mode::Exploit => {
                let score_shifted = score.abs_diff(self.baseline_score)
                    > (self.baseline_score / 2).max(REGIME_SCORE_SLACK_NS);
                let bpr_shifted = bpr.abs_diff(self.baseline_bpr) > REGIME_BPR_SLACK_BYTES
                    && (bpr > self.baseline_bpr.saturating_mul(2)
                        || self.baseline_bpr > bpr.saturating_mul(2));
                if score_shifted || bpr_shifted {
                    self.shift_streak = self.shift_streak.saturating_add(1);
                    if self.shift_streak >= REGIME_SHIFT_STREAK {
                        self.up_backoff = 1;
                        self.down_backoff = 1;
                        self.shift_streak = 0;
                    }
                } else {
                    self.shift_streak = 0;
                }
                self.baseline_score = smooth_u64(self.baseline_score, score);
                self.baseline_lag = smooth_u64(self.baseline_lag, peer_lag);
                self.baseline_bpr = smooth_u128(self.baseline_bpr, bpr);
                self.saturation_hold = self.saturation_hold.saturating_sub(1);
                self.up_wait = self.up_wait.saturating_add(1);
                self.down_wait = self.down_wait.saturating_add(1);

                let can_down = self.current > w_floor && self.saturation_hold == 0;
                if can_down && self.down_wait >= self.down_backoff {
                    self.down_wait = 0;
                    self.start_probe(self.prev_rung(self.current))
                } else if !distress && self.current < self.max_w && self.up_wait >= self.up_backoff {
                    self.up_wait = 0;
                    self.start_probe(self.next_rung(self.current))
                } else {
                    None
                }
            }
            Mode::DrainProbe { .. } => None,
            Mode::Probe { origin, origin_score, origin_lag } => {
                let lag_slack = (origin_lag * 30 / 100).max(PROBE_LAG_SLACK_NS);
                let lag_ok = peer_lag <= origin_lag.saturating_add(lag_slack);
                let upward = self.current > origin;
                let won = if upward {
                    let gain_req = (origin_score * 8 / 100).max(UP_GAIN_SLACK_NS);
                    !distress && lag_ok && score.saturating_add(gain_req) <= origin_score
                } else {
                    let tol = (origin_score * 5 / 100).max(DOWN_TOL_SLACK_NS);
                    !distress
                        && lag_ok
                        && self.current >= w_floor
                        && score <= origin_score.saturating_add(tol)
                };
                if won {
                    if upward {
                        self.up_backoff = 1;
                        self.down_backoff = 4;
                    } else {
                        self.down_backoff = 1;
                        self.up_backoff = 4;
                    }
                    self.enter_exploit(score, peer_lag, bpr);
                    None
                } else {
                    let old = self.current;
                    self.current = origin;
                    if upward {
                        self.up_backoff = self.up_backoff.saturating_mul(2).clamp(4, MAX_BACKOFF);
                        self.up_wait = 0;
                    } else {
                        self.down_backoff = self.down_backoff.saturating_mul(2).clamp(4, MAX_BACKOFF);
                        self.down_wait = 0;
                    }
                    self.drain_left = old;
                    self.mode = Mode::Exploit;
                    Some(origin)
                }
            }
        };

        self.reset_window(now);
        changed
    }

    fn jump_up(&mut self, target: u32, now: Timestamp) {
        self.current = target;
        self.mode = Mode::Warmup;
        self.drain_left = target;
        self.saturation_hold = SATURATION_HOLD_WINDOWS;
        self.up_backoff = 1;
        self.down_backoff = 4;
        self.up_wait = 0;
        self.down_wait = 0;
        self.shift_streak = 0;
        self.floor_streak = 0;
        self.distress_streak = 0;
        self.reset_window(now);
    }

    fn enter_exploit(&mut self, score: u64, peer_lag: u64, bpr: u128) {
        self.baseline_score = score;
        self.baseline_lag = peer_lag;
        self.baseline_bpr = bpr;
        self.mode = Mode::Exploit;
        self.up_wait = 0;
        self.down_wait = 0;
        self.shift_streak = 0;
    }

    fn start_probe(&mut self, candidate: u32) -> Option<u32> {
        if candidate == self.current {
            return None;
        }
        let origin = self.current;
        self.current = candidate;
        self.drain_left = origin.max(candidate);
        self.mode =
            Mode::DrainProbe { origin, origin_score: self.baseline_score, origin_lag: self.baseline_lag };
        Some(candidate)
    }

    fn abort_upward_probe(&mut self, origin: u32, now: Timestamp) -> u32 {
        let old = self.current;
        self.current = origin;
        self.up_backoff = self.up_backoff.saturating_mul(2).clamp(4, MAX_BACKOFF);
        self.up_wait = 0;
        self.drain_left = old;
        self.mode = Mode::Exploit;
        self.reset_window(now);
        origin
    }

    fn reset_window(&mut self, now: Timestamp) {
        self.win_start = now;
        self.win_slots = 0;
        self.win_bytes = 0;
        self.win_distress = false;
        self.high_res_streak = 0;
        self.residences.reset();
        self.peers.reset_window();
    }

    fn next_rung(&self, w: u32) -> u32 {
        self.ladder[..self.ladder_len].iter().copied().find(|&r| r > w).unwrap_or(self.max_w)
    }

    fn prev_rung(&self, w: u32) -> u32 {
        self.ladder[..self.ladder_len].iter().copied().take_while(|&r| r < w).last().unwrap_or(1)
    }

    fn ceil_rung(&self, needed: u32) -> u32 {
        self.ladder[..self.ladder_len].iter().copied().find(|&r| r >= needed).unwrap_or(self.max_w)
    }
}

fn build_ladder(max_w: u32) -> ([u32; MAX_LADDER], usize) {
    let mut ladder = [0u32; MAX_LADDER];
    let mut len = 0usize;
    let mut r = 1u32;
    while r <= max_w && len < MAX_LADDER {
        ladder[len] = r;
        len += 1;
        match r.checked_mul(2) {
            Some(next) => r = next,
            None => break,
        }
    }
    if len > 0 && ladder[len - 1] < max_w && len < MAX_LADDER {
        ladder[len] = max_w;
        len += 1;
    }
    (ladder, len)
}

#[cfg(test)]
mod tests;

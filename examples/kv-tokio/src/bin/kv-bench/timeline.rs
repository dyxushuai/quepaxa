//! What an open-loop run looked like over time: per-interval latency, the longest stretch
//! without a completed request, and how long throughput took to come back after a fault.
//!
//! Latency runs from a request's intended send time, so a generator that falls behind, or
//! a server that stalls, shows the full delay (no coordinated omission).

/// One request: when it was due, when it finished (microseconds from the run's start).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub due_us: u64,
    pub done_us: u64,
    pub outcome: Outcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Error,
    /// Still outstanding at the end; `done_us` is the end of the run.
    Unanswered,
}

impl Sample {
    pub fn latency_us(&self) -> u64 {
        self.done_us.saturating_sub(self.due_us)
    }
}

pub fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// Per-interval rows, by intended send time: `(start_us, completed, errors, p50, p99, max)`.
pub fn intervals(samples: &[Sample], width_us: u64) -> Vec<(u64, usize, usize, u64, u64, u64)> {
    let end = samples.iter().map(|s| s.due_us).max().unwrap_or(0);
    (0..=end / width_us)
        .map(|i| {
            let start = i * width_us;
            let in_bucket: Vec<&Sample> =
                samples.iter().filter(|s| s.due_us >= start && s.due_us < start + width_us).collect();
            let mut lat: Vec<u64> = in_bucket.iter().map(|s| s.latency_us()).collect();
            lat.sort_unstable();
            let completed = in_bucket.iter().filter(|s| s.outcome == Outcome::Ok).count();
            let errors = in_bucket.len() - completed;
            (
                start,
                completed,
                errors,
                percentile(&lat, 0.5),
                percentile(&lat, 0.99),
                lat.last().copied().unwrap_or(0),
            )
        })
        .collect()
}

/// The longest time, from `from_us` on, with no successful completion.
pub fn longest_gap_us(samples: &[Sample], from_us: u64, end_us: u64) -> u64 {
    let mut done: Vec<u64> = samples
        .iter()
        .filter(|s| s.outcome == Outcome::Ok && s.done_us >= from_us)
        .map(|s| s.done_us)
        .collect();
    done.sort_unstable();
    let mut prev = from_us;
    let mut gap = 0;
    for t in done.into_iter().chain([end_us]) {
        gap = gap.max(t.saturating_sub(prev));
        prev = t;
    }
    gap
}

/// Time from `fault_us` until throughput stays back: the end of the last one-second window
/// (starting at the fault, in 100 ms steps) whose completions fall below 80% of the rate in
/// the `fault_us` before it. `Some(0)` if none does; `None` if the run ends below it.
pub fn recovery_us(samples: &[Sample], fault_us: u64, end_us: u64) -> Option<u64> {
    const SEC: u64 = 1_000_000;
    let ok: Vec<u64> = samples.iter().filter(|s| s.outcome == Outcome::Ok).map(|s| s.done_us).collect();
    let before = ok.iter().filter(|&&t| t < fault_us).count() as f64 / (fault_us as f64 / SEC as f64);
    let mut last_low = None;
    let mut t = fault_us;
    while t + SEC <= end_us {
        let in_window = ok.iter().filter(|&&d| d >= t && d < t + SEC).count() as f64;
        if in_window < 0.8 * before {
            last_low = Some(t + SEC);
        }
        t += SEC / 10;
    }
    match last_low {
        None => Some(0),
        Some(end) if end + SEC / 10 > end_us => None,
        Some(end) => Some(end - fault_us),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request due every millisecond from 0 to `secs`, answered after 1 ms, except that
    /// nothing completes during `[stall_from, stall_to)`: requests due then finish at its end.
    fn run(secs: u64, stall_from: u64, stall_to: u64) -> Vec<Sample> {
        (0..secs * 1000)
            .map(|i| {
                let due = i * 1000;
                let done = if due + 1000 >= stall_from && due < stall_to { stall_to } else { due + 1000 };
                Sample { due_us: due, done_us: done, outcome: Outcome::Ok }
            })
            .collect()
    }

    #[test]
    fn a_two_second_stall_shows_as_a_two_second_gap_and_in_its_intervals() {
        let s = run(6, 2_000_000, 4_000_000);
        // The last completion before the stall is at 1.999 s, the next at 4 s.
        assert_eq!(longest_gap_us(&s, 0, 6_000_000), 2_001_000);
        let rows = intervals(&s, 1_000_000);
        assert_eq!(rows.len(), 6);
        assert!(rows[2].5 >= 1_999_000, "requests due when the stall began wait it out: {:?}", rows[2]);
        assert!(rows[1].4 < 2_000 && rows[5].4 < 2_000, "{rows:?}");
    }

    #[test]
    fn latency_runs_from_the_intended_send_time() {
        // Sent late by a generator that fell behind: due at 0, done at 500 ms.
        let s = Sample { due_us: 0, done_us: 500_000, outcome: Outcome::Ok };
        assert_eq!(s.latency_us(), 500_000);
    }

    #[test]
    fn unanswered_requests_count_until_the_end_and_are_not_completions() {
        let mut s = run(2, u64::MAX, u64::MAX);
        s.push(Sample { due_us: 1_500_000, done_us: 2_000_000, outcome: Outcome::Unanswered });
        let rows = intervals(&s, 1_000_000);
        assert_eq!((rows[1].1, rows[1].2, rows[1].5), (1000, 1, 500_000));
    }

    #[test]
    fn recovery_is_measured_from_the_fault() {
        // 1000/s before the fault at 3 s; nothing completes until 5 s, when the backlog does.
        let s = run(10, 3_000_000, 5_000_000);
        assert_eq!(recovery_us(&s, 3_000_000, 10_000_000), Some(2_000_000));
        assert_eq!(recovery_us(&run(10, u64::MAX, u64::MAX), 3_000_000, 10_000_000), Some(0));
        assert_eq!(recovery_us(&run(4, 3_000_000, 9_000_000), 3_000_000, 4_000_000), None);
    }
}

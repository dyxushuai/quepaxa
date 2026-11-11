//! Open-loop load over etcd's v3 KV gRPC API, so the same generator drives `kv-tokio` and
//! etcd. Each arrival is its own task: a stalled server never slows the arrivals, and every
//! request is timed from when it was due.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tonic::transport::{Channel, Endpoint};

use crate::pb::etcdserverpb::kv_client::KvClient;
use crate::pb::etcdserverpb::{PutRequest, RangeRequest};
use crate::timeline::{Outcome, Sample};
use crate::{GRACE, SplitMix, word};

pub struct Load {
    pub targets: Vec<SocketAddr>,
    pub rate: f64,
    /// Later rates: from each time (seconds into the run) on, that many requests per second
    /// (0 pauses), in time order.
    pub rate_at: Vec<(f64, f64)>,
    pub secs: f64,
    pub conns: usize,
    pub gets: f64,
    /// When the run starts on the wall clock, so another process can time faults against
    /// it; `None`: as soon as the clients exist.
    pub start_at: Option<SystemTime>,
    /// Seeds arrivals and keys: generators running side by side need different seeds.
    pub seed: u64,
}

impl Load {
    /// The rate at `at` seconds into the run, and when the next step starts.
    fn rate_in_force(&self, at: f64) -> (f64, Option<f64>) {
        let rate = self.rate_at.iter().rev().find(|(from, _)| *from <= at).map_or(self.rate, |s| s.1);
        (rate, self.rate_at.iter().map(|s| s.0).find(|&from| from > at))
    }

    /// The arrival after `at` seconds: exponential gaps at the rate in force give a Poisson
    /// process (open loop). A gap that would cross into the next step is drawn again at that
    /// step's rate (arrivals are memoryless, so each step stays a Poisson process); a paused
    /// step skips to the next one. None once paused for good.
    pub(crate) fn next_arrival(&self, rng: &mut SplitMix, mut at: f64) -> Option<f64> {
        loop {
            let (rate, step_end) = self.rate_in_force(at);
            let due = if rate > 0.0 { at - (1.0 - rng.unit()).ln() / rate } else { f64::INFINITY };
            match step_end {
                Some(end) if due > end => at = end,
                _ => return due.is_finite().then_some(due),
            }
        }
    }
}

/// Runs the load; returns every request's sample and the run's end (microseconds from start),
/// after which outstanding requests count as unanswered.
pub async fn run(load: &Load) -> (Vec<Sample>, u64) {
    // Lazy channels reconnect by themselves when a target dies and comes back.
    let clients: Vec<KvClient<Channel>> = (0..load.conns)
        .map(|c| {
            let addr = load.targets[c % load.targets.len()];
            let endpoint = Endpoint::from_shared(format!("http://{addr}")).expect("valid address");
            KvClient::new(endpoint.connect_timeout(GRACE).connect_lazy())
        })
        .collect();
    let samples = Arc::new(Mutex::new(Vec::new()));
    // Time zero is `start_at` even when the clients were ready late: arrivals then due are
    // sent at once and their delay counts.
    let start = match load.start_at {
        Some(at) => match at.duration_since(SystemTime::now()) {
            Ok(ahead) => Instant::now() + ahead,
            Err(late) => Instant::now() - late.duration(),
        },
        None => Instant::now(),
    };
    tokio::time::sleep_until(start.into()).await;
    let until = start + Duration::from_secs_f64(load.secs);
    let mut rng = SplitMix(load.seed);
    let mut handles = Vec::new();
    let mut at = 0.0;
    while let Some(due) = load.next_arrival(&mut rng, at) {
        at = due;
        let next = start + Duration::from_secs_f64(due);
        if next >= until {
            break;
        }
        tokio::time::sleep_until(next.into()).await;
        let due_us = (next - start).as_micros() as u64;
        let mut client = clients[handles.len() % clients.len()].clone();
        let (key, value, get) = (word(&mut rng), word(&mut rng), rng.unit() < load.gets);
        let samples = samples.clone();
        let task = tokio::spawn(async move {
            let ok = if get {
                client.range(RangeRequest { key: key.into(), ..Default::default() }).await.is_ok()
            } else {
                client
                    .put(PutRequest { key: key.into(), value: value.into(), ..Default::default() })
                    .await
                    .is_ok()
            };
            let done_us = start.elapsed().as_micros() as u64;
            let outcome = if ok { Outcome::Ok } else { Outcome::Error };
            samples.lock().unwrap().push(Sample { due_us, done_us, outcome });
        });
        handles.push((due_us, task));
    }
    let deadline = until + GRACE;
    while handles.iter().any(|(_, h)| !h.is_finished()) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let end_us = (Instant::now().min(deadline) - start).as_micros() as u64;
    // A task that finished pushed its own sample; one cancelled here never will.
    let mut unanswered = Vec::new();
    for (due_us, h) in handles {
        h.abort();
        if h.await.is_err() {
            unanswered.push(Sample { due_us, done_us: end_us, outcome: Outcome::Unanswered });
        }
    }
    let mut samples = std::mem::take(&mut *samples.lock().unwrap());
    samples.extend(unanswered);
    (samples, end_us)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(rate: f64, rate_at: Vec<(f64, f64)>) -> Load {
        Load { targets: Vec::new(), rate, rate_at, secs: 2.0, conns: 1, gets: 0.0, start_at: None, seed: 1 }
    }

    fn arrivals(l: &Load, until: f64) -> Vec<f64> {
        let (mut rng, mut at, mut out) = (SplitMix(l.seed), 0.0, Vec::new());
        while let Some(due) = l.next_arrival(&mut rng, at).filter(|d| *d < until) {
            out.push(due);
            at = due;
        }
        out
    }

    #[test]
    fn a_slow_step_does_not_swallow_a_later_fast_one() {
        let a = arrivals(&load(0.001, vec![(1.0, 1000.0)]), 2.0);
        let fast = a.iter().filter(|t| **t >= 1.0).count();
        assert!((900..1100).contains(&fast), "{fast} arrivals at 1000/s over a second");
    }

    #[test]
    fn nothing_arrives_while_paused() {
        let a = arrivals(&load(1000.0, vec![(0.5, 0.0), (1.0, 1000.0)]), 2.0);
        assert!(a.iter().all(|t| !(0.5..1.0).contains(t)), "an arrival inside the pause");
        assert!(a.iter().any(|t| *t > 1.0));
        assert!(arrivals(&load(1000.0, vec![(0.5, 0.0)]), 2.0).iter().all(|t| *t < 0.5));
    }
}

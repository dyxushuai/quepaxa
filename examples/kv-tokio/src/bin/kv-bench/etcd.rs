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
    pub secs: f64,
    pub conns: usize,
    pub gets: f64,
    /// When the run starts on the wall clock, so another process can time faults against
    /// it; `None`: as soon as the clients exist.
    pub start_at: Option<SystemTime>,
    /// Seeds arrivals and keys: generators running side by side need different seeds.
    pub seed: u64,
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
    let mut next = start;
    let mut handles = Vec::new();
    loop {
        // Exponential inter-arrival times give a Poisson process (open loop).
        next += Duration::from_secs_f64(-(1.0 - rng.unit()).ln() / load.rate);
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

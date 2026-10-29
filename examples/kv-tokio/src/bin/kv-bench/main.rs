//! Open-loop load generator for `kv-tokio` with the Go reference implementation's workload
//! shape: 17-byte commands (1-byte operation, 8-byte key, 8-byte value), 50/50 PUT/GET by
//! default, random keys from a 52-letter alphabet, Poisson arrivals.
//!
//! ```text
//! kv-bench --targets 127.0.0.1:7200,127.0.0.1:7201 --rate 20000 --secs 10 --conns 16 [--gets 0.5]
//! kv-bench --protocol etcd --targets 127.0.0.1:2379 --rate 5000 --secs 60 \
//!          [--timeline 100] [--fault-at 20] [--samples out.csv] [--start-at UNIX_MS]
//! ```
//!
//! Prints achieved throughput and latency percentiles. Results are measurements of one
//! machine and configuration, not production claims.
//!
//! `--protocol etcd` drives etcd's v3 KV gRPC API instead (`kv-tokio --etcd`, or etcd), with
//! a range for each GET and a put for each PUT. There latency runs from each request's
//! intended send time, and late arrivals are still sent: `--timeline MS` prints per-interval
//! latency, `--fault-at SECS` the longest stretch without a completion after that time and
//! how long throughput was down, and `--samples FILE` writes every request as
//! `due_us,done_us,outcome`. `--start-at` starts the run at that wall-clock time (Unix
//! milliseconds), so a fault injector on the same host can share its time zero.

mod etcd;
#[path = "../../pb.rs"]
mod pb;
mod timeline;

use timeline::percentile;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

fn word(rng: &mut SplitMix) -> String {
    (0..8).map(|_| LETTERS[(rng.next() % 52) as usize] as char).collect()
}

fn command(rng: &mut SplitMix, gets: f64) -> String {
    let key = word(rng);
    if rng.unit() < gets { format!("GET {key}\n") } else { format!("PUT {key} {}\n", word(rng)) }
}

/// How long a connect, the final `STATS`, or the wait for in-flight replies may take.
const GRACE: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Stats {
    latencies_us: Vec<u64>,
    errors: u64,
    /// Requests never answered within the grace period after the run (counted as errors).
    unanswered: u64,
    /// Arrivals skipped because the sender fell more than a second behind schedule.
    missed: u64,
}

async fn connection(
    addr: SocketAddr,
    rate: f64,
    until: Instant,
    gets: f64,
    seed: u64,
    stats: Arc<Mutex<Stats>>,
) {
    let Ok(Ok(s)) = tokio::time::timeout(GRACE, TcpStream::connect(addr)).await else {
        stats.lock().unwrap().errors += 1;
        return;
    };
    let _ = s.set_nodelay(true);
    let (r, mut w) = s.into_split();
    let sent = Arc::new(Mutex::new(VecDeque::<Instant>::new()));
    let reader_sent = sent.clone();
    let reader_stats = stats.clone();
    let reader = tokio::spawn(async move {
        let mut lines = BufReader::new(r).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Some(at) = reader_sent.lock().unwrap().pop_front() else { continue };
            let mut st = reader_stats.lock().unwrap();
            if line.starts_with("ERR") {
                st.errors += 1;
            } else {
                st.latencies_us.push(at.elapsed().as_micros() as u64);
            }
        }
    });
    let mut rng = SplitMix(seed);
    let mut next = Instant::now();
    let mut write_failed = false;
    loop {
        // Exponential inter-arrival times give a Poisson process (open loop).
        next += Duration::from_secs_f64(-(1.0 - rng.unit()).ln() / rate);
        if next >= until {
            break;
        }
        tokio::time::sleep_until(next.into()).await;
        if Instant::now() > next + Duration::from_secs(1) {
            stats.lock().unwrap().missed += 1;
            continue;
        }
        sent.lock().unwrap().push_back(Instant::now());
        // A server that stops reading must not hold the run past its end.
        let cmd = command(&mut rng, gets);
        let write = w.write_all(cmd.as_bytes());
        if !matches!(tokio::time::timeout_at((until + GRACE).into(), write).await, Ok(Ok(()))) {
            write_failed = true;
            break;
        }
    }
    // Give in-flight requests a grace period, then count the rest as unanswered.
    let grace = Instant::now() + GRACE;
    while !sent.lock().unwrap().is_empty() && Instant::now() < grace && !write_failed {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    reader.abort();
    let _ = w.shutdown().await;
    let left = sent.lock().unwrap().len() as u64;
    stats.lock().unwrap().unanswered += left;
}

/// The etcd run's summary, in the line protocol's format plus what happened over time.
fn report(
    samples: &[timeline::Sample],
    end_us: u64,
    rate: f64,
    secs: f64,
    timeline_ms: Option<u64>,
    fault_at: Option<f64>,
    samples_path: Option<&str>,
) {
    use timeline::Outcome;
    let mut lat: Vec<u64> =
        samples.iter().filter(|s| s.outcome == Outcome::Ok).map(|s| s.latency_us()).collect();
    lat.sort_unstable();
    let count = |o| samples.iter().filter(|s| s.outcome == o).count();
    println!(
        "offered={rate:.0}/s completed={} throughput={:.0}/s errors={} unanswered={} missed=0 p50={}us p90={}us p99={}us max={}us",
        lat.len(),
        lat.len() as f64 / secs,
        count(Outcome::Error),
        count(Outcome::Unanswered),
        percentile(&lat, 0.5),
        percentile(&lat, 0.9),
        percentile(&lat, 0.99),
        lat.last().copied().unwrap_or(0),
    );
    if let Some(at) = fault_at {
        let fault_us = (at * 1e6) as u64;
        // Up to the last arrival: completions fall off after it whatever the cluster does.
        let last_arrival = end_us.min((secs * 1e6) as u64);
        let gap = timeline::longest_gap_us(samples, fault_us, last_arrival);
        let degraded = timeline::degraded_us(samples, fault_us, last_arrival, 250_000);
        println!("fault_at={at}s longest_gap={}ms degraded={}ms", gap / 1000, degraded / 1000);
    }
    if let Some(ms) = timeline_ms {
        println!("interval_start_ms,completed,errors,p50_us,p99_us,max_us");
        for (start, ok, err, p50, p99, max) in timeline::intervals(samples, ms * 1000) {
            println!("{},{ok},{err},{p50},{p99},{max}", start / 1000);
        }
    }
    if let Some(path) = samples_path {
        let mut out = String::from("due_us,done_us,outcome\n");
        for s in samples {
            out += &format!("{},{},{:?}\n", s.due_us, s.done_us, s.outcome);
        }
        std::fs::write(path, out).expect("--samples: writable path");
    }
}

#[tokio::main]
async fn main() {
    let mut targets: Vec<SocketAddr> = Vec::new();
    let (mut rate, mut secs, mut conns, mut gets) = (1000.0, 10.0, 8usize, 0.5);
    let (mut protocol, mut samples_path, mut timeline_ms, mut fault_at, mut start_at) =
        (String::from("lines"), None, None, None, None);
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let v = it.next().unwrap_or_default();
        match flag.as_str() {
            "--targets" => {
                targets = v.split(',').map(|a| a.parse().expect("--targets: host:port list")).collect()
            }
            "--rate" => rate = v.parse().expect("--rate: requests per second"),
            "--secs" => secs = v.parse().expect("--secs: duration"),
            "--conns" => conns = v.parse().expect("--conns: connections"),
            "--gets" => gets = v.parse().expect("--gets: fraction of GETs"),
            "--protocol" => protocol = v,
            "--samples" => samples_path = Some(v),
            "--timeline" => timeline_ms = Some(v.parse::<u64>().expect("--timeline: milliseconds")),
            "--fault-at" => fault_at = Some(v.parse::<f64>().expect("--fault-at: seconds")),
            "--start-at" => {
                let ms = v.parse().expect("--start-at: Unix milliseconds");
                start_at = Some(std::time::UNIX_EPOCH + Duration::from_millis(ms));
            }
            other => panic!("unknown flag {other}"),
        }
    }
    assert!(!targets.is_empty(), "--targets is required");
    match protocol.as_str() {
        "lines" => assert!(start_at.is_none(), "--start-at needs --protocol etcd"),
        "etcd" => {
            let load = etcd::Load { targets, rate, secs, conns, gets, start_at };
            let (samples, end_us) = etcd::run(&load).await;
            report(&samples, end_us, rate, secs, timeline_ms, fault_at, samples_path.as_deref());
            return;
        }
        other => panic!("--protocol: lines or etcd, not {other}"),
    }
    let stats = Arc::new(Mutex::new(Stats::default()));
    let start = Instant::now();
    let until = start + Duration::from_secs_f64(secs);
    let tasks: Vec<_> = (0..conns)
        .map(|c| {
            let addr = targets[c % targets.len()];
            tokio::spawn(connection(addr, rate / conns as f64, until, gets, c as u64 + 1, stats.clone()))
        })
        .collect();
    for t in tasks {
        let _ = t.await;
    }
    {
        let mut st = stats.lock().unwrap();
        st.latencies_us.sort_unstable();
        let done = st.latencies_us.len();
        println!(
            "offered={rate:.0}/s completed={done} throughput={:.0}/s errors={} unanswered={} missed={} p50={}us p90={}us p99={}us max={}us",
            done as f64 / secs,
            st.errors,
            st.unanswered,
            st.missed,
            percentile(&st.latencies_us, 0.5),
            percentile(&st.latencies_us, 0.9),
            percentile(&st.latencies_us, 0.99),
            st.latencies_us.last().copied().unwrap_or(0),
        );
    }
    for addr in targets {
        let stats = async {
            let (r, mut w) = TcpStream::connect(addr).await.ok()?.into_split();
            w.write_all(b"STATS\n").await.ok()?;
            BufReader::new(r).lines().next_line().await.ok()?
        };
        match tokio::time::timeout(GRACE, stats).await {
            Ok(Some(line)) => println!("{addr} {line}"),
            _ => println!("{addr} no STATS within {GRACE:?}"),
        }
    }
}

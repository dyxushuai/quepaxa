//! Peer links and client pipelines over turmoil's simulated network and clock: links are
//! held, partitioned and throttled at exact simulated times, and every run with the same
//! seed takes the same course.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use quepaxa::{Action, Config, Input, NodeId, Replica, Timestamp};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc as tmpsc;
use tokio::time::{Instant, Sleep};
use turmoil::net::{TcpListener, TcpStream};

use super::{CHUNK, MAX_BACKOFF, MAX_PIPELINE, STALL, client, inbound, outbound, peer_link};
use crate::client::{Cmd, Reply};
use crate::node::{Ev, PeerFrame, PeerLink};

const PORT: u16 = 7100;

fn sim<'a>(latency: Option<Duration>, tcp_capacity: usize) -> turmoil::Sim<'a> {
    let mut b = turmoil::Builder::new();
    b.rng_seed(7).simulation_duration(Duration::from_secs(60)).tcp_capacity(tcp_capacity);
    if let Some(l) = latency {
        b.min_message_latency(l).max_message_latency(l);
    }
    b.build()
}

fn now() -> Duration {
    turmoil::sim_elapsed().expect("inside the simulation")
}

/// turmoil sends each write as one segment, whatever its size, and bounds only the
/// segments in flight; a kernel socket takes at most its free send buffer per write. This
/// caps every write at 64 KiB, so a write's progress depends on the reader.
struct SendBuffer(TcpStream);

impl AsyncWrite for SendBuffer {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let n = buf.len().min(64 << 10);
        Pin::new(&mut self.0).poll_write(cx, &buf[..n])
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// Runs `outbound` from "sender" to "receiver", fed by the returned queue.
fn sender(sim: &mut turmoil::Sim<'_>, cap: usize) -> tmpsc::Sender<Vec<u8>> {
    let (tx, rx) = tmpsc::channel(cap);
    let rx = RefCell::new(Some(rx));
    sim.host("sender", move || {
        let rx = rx.borrow_mut().take().expect("the sender is not restarted");
        async move {
            outbound(0, async || Ok(SendBuffer(TcpStream::connect(("receiver", PORT)).await?)), rx).await;
            Ok(())
        }
    });
    tx
}

/// Accepts every connection on "receiver", recording when, and drains each one.
fn draining_receiver(sim: &mut turmoil::Sim<'_>) -> Rc<RefCell<Vec<Duration>>> {
    let accepted = Rc::new(RefCell::new(Vec::new()));
    let log = accepted.clone();
    sim.host("receiver", move || {
        let log = log.clone();
        async move {
            let listener = TcpListener::bind(("0.0.0.0", PORT)).await?;
            loop {
                let (mut s, _) = listener.accept().await?;
                log.borrow_mut().push(now());
                tokio::spawn(async move { tokio::io::copy(&mut s, &mut tokio::io::sink()).await });
            }
        }
    });
    accepted
}

#[test]
fn a_held_link_is_redialed() -> turmoil::Result {
    let mut sim = sim(Some(Duration::from_millis(1)), 64);
    let tx = sender(&mut sim, 4096);
    let accepted = draining_receiver(&mut sim);
    sim.client("driver", async move {
        tokio::spawn(async move { while tx.send(vec![0; 64 << 10]).await.is_ok() {} });
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(accepted.borrow().len(), 1, "frames flow on one connection");
        let held = now();
        turmoil::hold("sender", "receiver");
        // A hold also holds the redial's SYN, so it is released once the old connection
        // has stalled past `STALL`; a sender still waiting on that connection never redials.
        tokio::time::sleep(STALL + Duration::from_millis(500)).await;
        turmoil::release("sender", "receiver");
        tokio::time::sleep(STALL + MAX_BACKOFF).await;
        let redial = accepted.borrow().get(1).map(|t| *t - held);
        assert!(
            redial.is_some_and(|d| d <= STALL + MAX_BACKOFF),
            "a held link was not redialed within STALL plus the backoff: {redial:?}"
        );
        Ok(())
    });
    sim.run()
}

#[test]
fn frames_flow_again_soon_after_a_partition_heals() -> turmoil::Result {
    let mut sim = sim(Some(Duration::from_millis(1)), 64);
    let tx = sender(&mut sim, 4096);
    let marker = b"after the repair".as_slice();
    let arrived = Rc::new(RefCell::new(None));
    let log = arrived.clone();
    sim.host("receiver", move || {
        let log = log.clone();
        async move {
            let listener = TcpListener::bind(("0.0.0.0", PORT)).await?;
            let (events, rx) = mpsc::sync_channel(65_536);
            tokio::spawn(async move {
                while let Ok((s, _)) = listener.accept().await {
                    tokio::spawn(inbound(s, events.clone()));
                }
            });
            loop {
                for ev in rx.try_iter() {
                    if let Ev::Peer { frame: PeerFrame::Msg(m), .. } = ev
                        && m == marker
                    {
                        log.borrow_mut().get_or_insert(now());
                    }
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    });
    sim.client("driver", async move {
        // Like `Node::send`: a frame every 10 ms, dropped while the queue is full.
        let feed = tx.clone();
        tokio::spawn(async move {
            loop {
                let _ = feed.try_send(PeerFrame::Msg(vec![1; 512].into()).encode());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        turmoil::partition("sender", "receiver");
        tokio::time::sleep(Duration::from_secs(10)).await;
        turmoil::repair("sender", "receiver");
        let repaired = now();
        tx.send(PeerFrame::Msg(marker.into()).encode()).await?;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let after = arrived.borrow().map(|t| t - repaired);
        assert!(
            after.is_some_and(|d| d <= MAX_BACKOFF + Duration::from_millis(100)),
            "a frame sent after the repair took {after:?} to arrive"
        );
        Ok(())
    });
    sim.run()
}

#[test]
fn a_large_frame_on_a_slow_draining_link_is_not_cut_off() -> turmoil::Result {
    const LEN: usize = 16 << 20;
    // Four 64 KiB segments in flight: the sender waits on the reader for every chunk.
    let mut sim = sim(Some(Duration::from_millis(1)), 4);
    let tx = sender(&mut sim, 4);
    sim.client("receiver", async move {
        let listener = TcpListener::bind(("0.0.0.0", PORT)).await?;
        tx.send(vec![7; LEN]).await?;
        let (mut s, _) = listener.accept().await?;
        let redials = tokio::spawn(async move { listener.accept().await.is_ok() });
        let started = now();
        // About 3 MiB/s: the frame takes far longer than `STALL`, but bytes keep moving.
        let (mut got, mut buf) = (0, vec![0; CHUNK]);
        while got < 4 + 4 + LEN {
            // A given-up connection may never show EOF here: turmoil queues the FIN behind
            // a full receive queue until another segment arrives. Silence counts as EOF.
            let n = tokio::time::timeout(STALL, s.read(&mut buf)).await.unwrap_or(Ok(0))?;
            assert!(n > 0, "the connection was given up after {got} bytes");
            got += n;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(now() - started > STALL, "the link was not slow enough to test anything");
        assert!(!redials.is_finished(), "the frame was delivered on one connection only");
        Ok(())
    });
    sim.run()
}

#[test]
fn a_client_that_stops_reading_is_bounded_and_answered_in_order() -> turmoil::Result {
    const SENT: usize = 3 * MAX_PIPELINE;
    // turmoil's default latencies: every segment takes 0-100 ms.
    let mut sim = sim(None, 64);
    sim.client("server", async {
        let listener = TcpListener::bind(("0.0.0.0", PORT)).await?;
        let (events, rx) = mpsc::sync_channel(4 * MAX_PIPELINE);
        let (r, w) = listener.accept().await?.0.into_split();
        tokio::spawn(client(r, w, events));
        // The node answers nothing until the connection has backed up.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let mut held: Vec<Ev> = rx.try_iter().collect();
        assert!((MAX_PIPELINE..=MAX_PIPELINE + 2).contains(&held.len()), "{} requests taken in", held.len());
        // Then it answers out of order, each request with its own key.
        let mut answered = 0;
        while answered < SENT {
            for ev in held.drain(..).rev() {
                let Ev::Client { cmd: Cmd::Read { range, .. }, reply } = ev else { panic!("a read") };
                let _ = reply.send(Reply::Text(String::from_utf8(range.key)?));
                answered += 1;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
            held.extend(rx.try_iter());
        }
        Ok(())
    });
    sim.client("client", async {
        let (r, mut w) = TcpStream::connect(("server", PORT)).await?.into_split();
        tokio::spawn(async move {
            for i in 0..SENT {
                w.write_all(format!("GET k{i}\n").as_bytes()).await?;
            }
            std::io::Result::Ok(())
        });
        tokio::time::sleep(Duration::from_secs(5)).await;
        let mut lines = BufReader::new(r).lines();
        for i in 0..SENT {
            assert_eq!(lines.next_line().await?, Some(format!("k{i}")), "replies keep request order");
        }
        Ok(())
    });
    sim.run()
}

/// The slow link of `a_snapshot_on_a_slow_link_delays_no_consensus_message`: bytes per
/// second, and one-way latency (a WAN path). The bulk connection's chunks still queue ahead
/// of messages on the link itself; a chunk (about 21 ms here) stays well under the commit
/// latency over such a path, which is what the replicas' timers scale with.
const LINK_RATE: f64 = (3 << 20) as f64;
const LINK_LATENCY: Duration = Duration::from_millis(25);

/// A slow link shared by every connection between two hosts: each write (at most 64 KiB)
/// waits until the bytes written before it on the link, on any connection, have gone out.
struct SlowLink {
    s: TcpStream,
    /// When the link is free again.
    free: Arc<Mutex<Instant>>,
    wait: Option<(Pin<Box<Sleep>>, usize)>,
}

impl AsyncWrite for SlowLink {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        let (sleep, granted) = this.wait.get_or_insert_with(|| {
            let n = buf.len().min(64 << 10);
            let mut free = this.free.lock().unwrap();
            *free = (*free).max(Instant::now()) + Duration::from_secs_f64(n as f64 / LINK_RATE);
            (Box::pin(tokio::time::sleep_until(*free)), n)
        });
        std::task::ready!(sleep.as_mut().poll(cx));
        let n = (*granted).min(buf.len());
        let written = Pin::new(&mut this.s).poll_write(cx, &buf[..n]);
        if written.is_ready() {
            this.wait = None;
        }
        written
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.s).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.s).poll_shutdown(cx)
    }
}

/// What the replicas of `a_snapshot_on_a_slow_link_delays_no_consensus_message` report.
#[derive(Default)]
struct Seen {
    /// Per replica: its `suspicions` and `hedged` metrics.
    metrics: [(u64, u64); 3],
    /// Proposals of replica 1 committed.
    committed: u64,
    /// When replica 1 received the snapshot.
    snapshot_at: Option<Duration>,
}

/// When replica 0 sends replica 1 its snapshot.
const SNAPSHOT_AT: Duration = Duration::from_secs(2);

/// Replica `id` of three on host `r{id}`, driven like `Node` with instant persists: replica 1
/// proposes every 10 ms (forwarding to the leader, 0), and the leader sends it a 16 MiB
/// snapshot at `SNAPSHOT_AT`. Every link is a `SlowLink`.
fn replica_host(sim: &mut turmoil::Sim<'_>, id: u32, seen: Arc<Mutex<Seen>>) {
    sim.host(format!("r{id}"), move || {
        let seen = seen.clone();
        async move {
            let listener = TcpListener::bind(("0.0.0.0", PORT)).await?;
            let (events, rx) = mpsc::sync_channel(65_536);
            tokio::spawn(async move {
                while let Ok((s, _)) = listener.accept().await {
                    tokio::spawn(inbound(s, events.clone()));
                }
            });
            let peers: HashMap<u32, PeerLink> = (0..3)
                .filter(|p| *p != id)
                .map(|p| {
                    let free = Arc::new(Mutex::new(Instant::now()));
                    let connect = move || {
                        let free = free.clone();
                        async move {
                            let s = TcpStream::connect((format!("r{p}"), PORT)).await?;
                            Ok(SlowLink { s, free, wait: None })
                        }
                    };
                    (p, peer_link(id, connect))
                })
                .collect();
            let ts = || Timestamp(now().as_nanos() as u64);
            let cfg = Config::new([1; 16], NodeId(id), (0..3).map(NodeId).collect(), [id as u8 + 1; 32]);
            let mut replica = Replica::new(cfg, [], ts())?;
            let (mut proposed_at, mut snapshot_sent) = (Duration::ZERO, false);
            loop {
                if replica.next_deadline().is_some_and(|t| t <= ts()) {
                    let _ = replica.handle_timeout(ts());
                }
                drive(&mut replica, &peers, &seen, id);
                for ev in rx.try_iter() {
                    match ev {
                        Ev::Peer { frame: PeerFrame::Msg(m), .. } => {
                            let _ = replica.handle(ts(), Input::Message(m));
                        }
                        Ev::Peer { frame: PeerFrame::Snapshot(_), .. } => {
                            seen.lock().unwrap().snapshot_at.get_or_insert(now());
                        }
                        _ => {}
                    }
                    drive(&mut replica, &peers, &seen, id);
                }
                if id == 1 && now() >= proposed_at + Duration::from_millis(10) {
                    proposed_at = now();
                    let _ = replica.propose(ts(), Bytes::from_static(b"put"));
                    drive(&mut replica, &peers, &seen, id);
                }
                if id == 0 && !snapshot_sent && now() >= SNAPSHOT_AT {
                    snapshot_sent = true;
                    peers[&1].send(PeerFrame::Snapshot(vec![7; 16 << 20].into()));
                }
                let m = replica.metrics();
                seen.lock().unwrap().metrics[id as usize] = (m.suspicions, m.hedged);
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    });
}

/// Takes every action, each persist durable at once.
fn drive(replica: &mut Replica, peers: &HashMap<u32, PeerLink>, seen: &Mutex<Seen>, id: u32) {
    let mut actions = Vec::new();
    let mut persisted = Vec::new();
    loop {
        replica.poll_actions(&mut actions);
        if actions.is_empty() && persisted.is_empty() {
            return;
        }
        for a in actions.drain(..) {
            match a {
                Action::Send(o) => peers[&o.to.0].send(PeerFrame::Msg(o.bytes)),
                Action::Persist { id: p, then, .. } => {
                    for o in then {
                        peers[&o.to.0].send(PeerFrame::Msg(o.bytes));
                    }
                    persisted.push(p);
                }
                Action::Committed { .. } if id == 1 => seen.lock().unwrap().committed += 1,
                _ => {}
            }
        }
        // The replica refuses input while actions are pending: they are all taken above.
        for p in persisted.drain(..) {
            let _ = replica.handle(Timestamp(now().as_nanos() as u64), Input::Persisted(p));
        }
    }
}

#[test]
fn a_snapshot_on_a_slow_link_delays_no_consensus_message() -> turmoil::Result {
    let mut sim = sim(Some(LINK_LATENCY), 64);
    let seen = Arc::new(Mutex::new(Seen::default()));
    for id in 0..3 {
        replica_host(&mut sim, id, seen.clone());
    }
    let watch = seen.clone();
    sim.client("driver", async move {
        // The cluster has settled: what its start caused does not count.
        tokio::time::sleep(SNAPSHOT_AT - Duration::from_millis(100)).await;
        let (before, committed) = {
            let s = watch.lock().unwrap();
            (s.metrics, s.committed)
        };
        while watch.lock().unwrap().snapshot_at.is_none() && now() < Duration::from_secs(30) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Past `hedge_max` as well: a suspicion the transfer caused has shown by now.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let s = watch.lock().unwrap();
        let took = s.snapshot_at.map(|t| t - SNAPSHOT_AT);
        assert!(took.is_some_and(|t| t > STALL), "the link was not slow enough to test anything: {took:?}");
        let grew: Vec<_> = before.iter().zip(&s.metrics).map(|(b, a)| (a.0 - b.0, a.1 - b.1)).collect();
        let committed = s.committed - committed;
        eprintln!("snapshot took {took:?}; (suspicions, hedged) grew by {grew:?}; {committed} committed");
        assert!(committed > 100, "replica 1's writes kept committing: {committed}");
        assert_eq!(grew, [(0, 0); 3], "no replica suspected the leader or hedged during the transfer");
        Ok(())
    });
    sim.run()
}

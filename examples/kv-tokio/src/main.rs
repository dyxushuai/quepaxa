//! A replicated key-value store over QuePaxa: the reference runtime for `quepaxa`.
//!
//! ```text
//! kv-tokio --id 0 --members 0=127.0.0.1:7100,1=127.0.0.1:7101,2=127.0.0.1:7102 \
//!          --client 127.0.0.1:7200 --data /tmp/kv0 [--snapshot-every 1000] [--leader-order 2,0,1] \
//!          [--peers 3=127.0.0.1:7103] [--etcd 127.0.0.1:2379] [--retransmit-ms 400] [--hedge-max-ms 3000]
//!          [--placement on] [--placement-periods 30000,120000]
//! ```
//!
//! `--retransmit-ms` and `--hedge-max-ms` set the replica's `retransmit_interval` and
//! `hedge_max`; raise them above the longest round trip and commit path on a WAN.
//! `--placement on` lets the leader move to the replica where writes commit fastest (set it
//! on every replica); `--placement-periods` sets its sustained period and hold in ms.
//! `STATS` ends with `LEADERS <ids>`, the leader order in force.
//!
//! `--members` are the configured members; `--peers` are replicas that may join later (a
//! joining replica runs with the same `--members` and finds its own address in `--peers`).
//!
//! Clients speak lines: `PUT <key> <value>` → `OK`, `GET <key>` (linearizable, through a
//! read barrier) → `VALUE <v>` | `NONE`, `STALE <key>` (local, possibly stale), `STATS` →
//! `STATS <counters>`, `RECONFIGURE <ids> [LEADERS <ids>]` (comma-separated) → `OK` once
//! the change is decided, and `LEADERS <ids>` (every member in force, preferred leader first)
//! → `OK` once the new leader order is decided. Requests on one connection may be pipelined; replies come back in
//! order.
//!
//! With `--etcd`, the same replica also serves etcd v3's KV gRPC API (`Range`, `Put`,
//! `DeleteRange`, single-level `Txn`), so etcd's clients and tools work against it; see
//! `src/grpc.rs` for what is refused.
//!
//! A replica removed by a change keeps serving until the new members no longer need it,
//! then exits with status 0 (`Action::Retired`); its data directory may then be deleted.
//! `RECONFIGURE` only names replicas with a known address.
//!
//! Trusted networks only: peer connections are not authenticated or encrypted, and the
//! peer id is whatever the connecting side claims. Put replicas on a private network or
//! behind mTLS (e.g. a mesh sidecar) before exposing them.

mod client;
mod grpc;
mod kv;
mod node;
mod ops;
mod pb;
mod storage;
mod sync;
#[cfg(all(test, not(feature = "shuttle")))]
mod turmoil_tests;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use quepaxa::{Config, NodeId, Replica, Timestamp};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot::error::TryRecvError;
use tokio::sync::{mpsc as tmpsc, oneshot};

use crate::sync::mpsc::{self, SyncSender, TrySendError};

/// The event loop and the snapshot thread allocate and free heavily at once; with the system
/// allocator, cross-thread frees made compaction take 2-8 ms instead of under 2 ms.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use client::{Cmd, Reply};
use node::{Ev, MAX_FRAME, Node, PeerFrame, PeerLink};
use ops::{RangeReq, Request};
use storage::Disk;

struct Args {
    id: u32,
    members: Vec<(u32, SocketAddr)>,
    /// Replicas that may join later.
    peers: Vec<(u32, SocketAddr)>,
    client: SocketAddr,
    /// Where to serve etcd's v3 KV gRPC API, if anywhere.
    etcd: Option<SocketAddr>,
    data: PathBuf,
    snapshot_every: u64,
    leader_order: Vec<NodeId>,
    retransmit_ms: Option<u64>,
    hedge_max_ms: Option<u64>,
    placement: bool,
    placement_periods: Option<(u64, u64)>,
}

const MILLI: u64 = 1_000_000;

fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let (mut id, mut members, mut client, mut data, mut every) = (None, Vec::new(), None, None, 10_000);
    let (mut leader_order, mut peers, mut etcd) = (Vec::new(), Vec::new(), None);
    let (mut retransmit_ms, mut hedge_max_ms, mut placement) = (None, None, false);
    let mut placement_periods = None;
    let addrs = |flag: &str, value: &str| -> Result<Vec<(u32, SocketAddr)>, String> {
        value
            .split(',')
            .map(|m| {
                let (i, a) = m.split_once('=').ok_or(format!("{flag}: expected id=addr"))?;
                Ok((
                    i.parse().map_err(|e| format!("{flag}: {e}"))?,
                    a.parse().map_err(|e| format!("{flag}: {e}"))?,
                ))
            })
            .collect()
    };
    while let Some(flag) = it.next() {
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--id" => id = Some(value.parse().map_err(|e| format!("--id: {e}"))?),
            "--members" => members = addrs("--members", &value)?,
            "--peers" => peers = addrs("--peers", &value)?,
            "--client" => client = Some(value.parse().map_err(|e| format!("--client: {e}"))?),
            "--data" => data = Some(PathBuf::from(value)),
            "--etcd" => etcd = Some(value.parse().map_err(|e| format!("--etcd: {e}"))?),
            "--snapshot-every" => every = value.parse().map_err(|e| format!("--snapshot-every: {e}"))?,
            "--leader-order" => {
                for i in value.split(',') {
                    leader_order.push(NodeId(i.parse().map_err(|e| format!("--leader-order: {e}"))?));
                }
            }
            "--retransmit-ms" => {
                retransmit_ms = Some(value.parse().map_err(|e| format!("--retransmit-ms: {e}"))?)
            }
            "--hedge-max-ms" => {
                hedge_max_ms = Some(value.parse().map_err(|e| format!("--hedge-max-ms: {e}"))?)
            }
            "--placement" => {
                placement = match value.as_str() {
                    "on" => true,
                    "off" => false,
                    _ => return Err("--placement: expected on or off".to_owned()),
                }
            }
            "--placement-periods" => {
                let ms = |v: &str| v.parse::<u64>().map_err(|e| format!("--placement-periods: {e}"));
                let (sustain, hold) =
                    value.split_once(',').ok_or("--placement-periods: expected SUSTAIN_MS,HOLD_MS")?;
                placement_periods = Some((ms(sustain)?, ms(hold)?));
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    let mut ids: Vec<u32> = members.iter().chain(&peers).map(|(i, _)| *i).collect();
    ids.sort_unstable();
    if let Some(w) = ids.windows(2).find(|w| w[0] == w[1]) {
        return Err(format!("replica {} is listed twice in --members/--peers", w[0]));
    }
    Ok(Args {
        id: id.ok_or("--id is required")?,
        client: client.ok_or("--client is required")?,
        etcd,
        data: data.ok_or("--data is required")?,
        members,
        peers,
        snapshot_every: every,
        leader_order,
        retransmit_ms,
        hedge_max_ms,
        placement,
        placement_periods,
    })
}

/// The replica's configuration: what the flags set, `Config::new`'s defaults otherwise.
fn config(args: &Args) -> Config {
    let members: Vec<NodeId> = args.members.iter().map(|(i, _)| NodeId(*i)).collect();
    let mut cluster_id = [0u8; 16];
    cluster_id[..8].copy_from_slice(b"kv-tokio");
    let mut cfg = Config::new(cluster_id, NodeId(args.id), members, seed());
    cfg.leader_order = args.leader_order.clone();
    // Room for seconds of a busy node's requests: with the default (4096), a CPU
    // stall of a few hundred milliseconds under 40k requests/s fails requests with `Busy`
    // instead of queueing them.
    cfg.max_outstanding_proposals = 1 << 16;
    if let Some(ms) = args.retransmit_ms {
        cfg.retransmit_interval = ms.saturating_mul(MILLI);
    }
    if let Some(ms) = args.hedge_max_ms {
        cfg.hedge_max = ms.saturating_mul(MILLI);
    }
    cfg.placement = args.placement;
    if let Some((sustain, hold)) = args.placement_periods {
        cfg.placement_sustain = sustain.saturating_mul(MILLI);
        cfg.placement_hold = hold.saturating_mul(MILLI);
    }
    cfg
}

/// Reads one length-prefixed frame; the buffer grows with the bytes actually received, so
/// an advertised length alone cannot reserve memory.
async fn read_frame(s: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Bytes> {
    let len = s.read_u32_le().await? as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::other("frame too large"));
    }
    // Grows with the bytes that arrive, not with the advertised length.
    let mut buf = Vec::new();
    s.take(len as u64).read_to_end(&mut buf).await?;
    if buf.len() != len {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    Ok(Bytes::from(buf))
}

/// Queues an event for the replica thread, waiting (without blocking the runtime) while
/// the bounded queue is full; false once the replica thread is gone.
async fn push(events: &SyncSender<Ev>, mut ev: Ev) -> bool {
    loop {
        match events.try_send(ev) {
            Ok(()) => return true,
            Err(TrySendError::Full(back)) => {
                ev = back;
                tokio::time::sleep(Duration::from_micros(200)).await;
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

/// Most time one chunk of a peer frame may take to be accepted by the socket.
const STALL: Duration = Duration::from_secs(2);
/// A frame is written in chunks of this size, each within `STALL`: a large snapshot on a
/// slow but draining link is not cut off, a link that drains nothing is.
const CHUNK: usize = 64 << 10;

/// Whether `io` finished, successfully, within `STALL`.
async fn within_stall(io: impl Future<Output = std::io::Result<()>>) -> bool {
    matches!(tokio::time::timeout(STALL, io).await, Ok(Ok(())))
}

/// A bulk frame (a snapshot) is written in chunks of this size, each flushed within `STALL`
/// and followed by a pause as long as it took. On a link whose bottleneck is one FIFO
/// queue, the consensus connection's frames then wait behind at most one chunk and get at
/// least half the link. Over 3 MiB/s with 1 ms latency, 64 KiB chunks (21 ms each) still
/// outlast the commit latency and the followers suspect the leader; much smaller ones
/// lose time to the timer's 1 ms granularity on every pause.
const BULK_CHUNK: usize = 32 << 10;

/// Writes one length-prefixed frame, giving up if the socket stops taking bytes for `STALL`.
/// A `bulk` frame is paced (see `BULK_CHUNK`).
async fn send_frame(w: &mut (impl AsyncWrite + Unpin), frame: &[u8], bulk: bool) -> bool {
    if !within_stall(w.write_u32_le(frame.len() as u32)).await {
        return false;
    }
    if !bulk {
        for chunk in frame.chunks(CHUNK) {
            if !within_stall(w.write_all(chunk)).await {
                return false;
            }
        }
        return true;
    }
    for chunk in frame.chunks(BULK_CHUNK) {
        let started = tokio::time::Instant::now();
        if !within_stall(async {
            w.write_all(chunk).await?;
            w.flush().await
        })
        .await
        {
            return false;
        }
        tokio::time::sleep(started.elapsed()).await;
    }
    true
}

/// `outbound`'s redial backoff.
const MIN_BACKOFF: Duration = Duration::from_millis(100);
const MAX_BACKOFF: Duration = Duration::from_secs(1);

/// Keeps a connection to `peer`, reconnecting forever with exponential backoff (reset once
/// a connection carries a frame); lost frames are retransmitted by the protocol. A connect
/// that takes longer than `STALL`, or a socket that takes no bytes for that long, gives the
/// connection up: on a link that silently drops packets (a partition), TCP would otherwise
/// keep retrying with its own backoff and resume seconds after the link heals. A `bulk`
/// connection paces its frames (see `BULK_CHUNK`).
async fn outbound<S, C>(me: u32, connect: impl Fn() -> C, mut rx: tmpsc::Receiver<Vec<u8>>, bulk: bool)
where
    S: AsyncWrite + Unpin,
    C: Future<Output = std::io::Result<S>>,
{
    let mut backoff = MIN_BACKOFF;
    loop {
        if let Ok(Ok(s)) = tokio::time::timeout(STALL, connect()).await {
            // Frames queued while the previous batch was written go out in one syscall.
            let mut w = BufWriter::with_capacity(CHUNK, s);
            let hello = send_frame(&mut w, &me.to_le_bytes(), false).await && within_stall(w.flush()).await;
            while hello && let Some(frame) = rx.recv().await {
                let mut ok = send_frame(&mut w, &frame, bulk).await;
                while ok && let Ok(frame) = rx.try_recv() {
                    ok = send_frame(&mut w, &frame, bulk).await;
                }
                if !ok || !within_stall(w.flush()).await {
                    break;
                }
                backoff = MIN_BACKOFF;
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Frames queued for one peer's consensus connection.
const MSG_QUEUE: usize = 4096;
/// Snapshots queued for one peer's bulk connection besides the one being sent: each may be
/// up to `MAX_FRAME`, and a peer that asks again meanwhile can wait for the next.
const BULK_QUEUE: usize = 1;

/// Starts the two connections to a peer: consensus messages on one and snapshots, paced,
/// on the other, so a message never waits for a whole snapshot. The peer tells them apart
/// by frame kind only.
fn peer_link<S, C>(me: u32, connect: impl Fn() -> C + Clone + Send + 'static) -> PeerLink
where
    S: AsyncWrite + Unpin + Send + 'static,
    C: Future<Output = std::io::Result<S>> + Send + 'static,
{
    let (msgs, rx) = tmpsc::channel(MSG_QUEUE);
    tokio::spawn(outbound(me, connect.clone(), rx, false));
    let (bulk, rx) = tmpsc::channel(BULK_QUEUE);
    tokio::spawn(outbound(me, connect, rx, true));
    PeerLink::new(msgs, bulk)
}

/// Connects to a peer for `outbound`.
async fn dial(addr: SocketAddr) -> std::io::Result<TcpStream> {
    let s = TcpStream::connect(addr).await?;
    let _ = s.set_nodelay(true);
    Ok(s)
}

async fn inbound(s: impl AsyncRead + Unpin, events: SyncSender<Ev>) {
    let mut s = BufReader::with_capacity(64 << 10, s);
    let Ok(Ok(hello)) = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut s)).await else {
        return;
    };
    let Ok(from) = <[u8; 4]>::try_from(&hello[..]).map(u32::from_le_bytes) else { return };
    while let Ok(frame) = read_frame(&mut s).await {
        if let Some(frame) = PeerFrame::decode(frame)
            && !push(&events, Ev::Peer { from, frame }).await
        {
            return;
        }
    }
}

fn parse_cmd(line: &str) -> Option<Cmd> {
    let ids = |s: &str| s.split(',').map(|i| i.parse().map(NodeId)).collect::<Result<Vec<_>, _>>().ok();
    let words: Vec<&str> = line.split_whitespace().collect();
    match words[..] {
        ["PUT", k, v] => Some(Cmd::Write(Request::Put { key: k.into(), value: v.into(), prev_kv: false })),
        ["GET", k] => Some(Cmd::Read { range: RangeReq::key(k.as_bytes()), linearizable: true }),
        ["STALE", k] => Some(Cmd::Read { range: RangeReq::key(k.as_bytes()), linearizable: false }),
        ["STATS"] => Some(Cmd::Stats),
        ["RECONFIGURE", members] => Some(Cmd::Reconfigure(ids(members)?, Vec::new())),
        ["RECONFIGURE", members, "LEADERS", order] => Some(Cmd::Reconfigure(ids(members)?, ids(order)?)),
        ["LEADERS", order] => Some(Cmd::LeaderOrder(ids(order)?)),
        _ => None,
    }
}

/// Longest request line; a longer one is answered with an error and closes the connection.
const MAX_LINE: usize = 1 << 20;
/// Requests one connection may have queued for replies (plus the one being written and the
/// one being read); past that, its requests are no longer read (TCP pushes back on the
/// client) until replies drain.
const MAX_PIPELINE: usize = 1024;

/// Pipelined client connection: requests go to the node as they arrive, replies are
/// written in request order.
async fn client(
    r: impl AsyncRead + Unpin,
    w: impl AsyncWrite + Unpin + Send + 'static,
    events: SyncSender<Ev>,
) {
    let (tx, mut replies) = tmpsc::channel::<oneshot::Receiver<Reply>>(MAX_PIPELINE);
    let writer = tokio::spawn(async move {
        // Replies already available are written together; the buffer is flushed before
        // waiting on one that is not.
        let mut w = BufWriter::new(w);
        while let Some(mut rx) = replies.recv().await {
            let line = match rx.try_recv() {
                Ok(reply) => client::line(reply),
                Err(TryRecvError::Empty) => {
                    if w.flush().await.is_err() {
                        return;
                    }
                    rx.await.map_or_else(|_| "ERR node stopped".to_owned(), client::line)
                }
                Err(TryRecvError::Closed) => "ERR node stopped".to_owned(),
            };
            let written = w.write_all(line.as_bytes()).await.and(w.write_all(b"\n").await);
            if written.is_err() || (replies.is_empty() && w.flush().await.is_err()) {
                return;
            }
        }
    });
    let mut r = BufReader::new(r);
    let mut line = Vec::new();
    loop {
        line.clear();
        if !matches!((&mut r).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line).await, Ok(1..)) {
            break;
        }
        let (reply, rx) = oneshot::channel();
        if line.len() > MAX_LINE && line.last() != Some(&b'\n') {
            let _ = reply.send(Reply::Text(format!("ERR line longer than {MAX_LINE} bytes")));
            let _ = tx.send(rx).await;
            drop(tx);
            // The writer closes its side once the error is out; the rest of the input is
            // read and dropped meanwhile, as closing with unread input would reset the
            // connection (macOS, Windows) and could discard the error.
            let mut sink = tokio::io::sink();
            let drain = tokio::io::copy(&mut r, &mut sink);
            let _ = tokio::time::timeout(Duration::from_secs(1), async { tokio::join!(writer, drain) }).await;
            return;
        }
        let text = line.strip_suffix(b"\n").map_or(&line[..], |l| l.strip_suffix(b"\r").unwrap_or(l));
        match std::str::from_utf8(text).ok().and_then(parse_cmd) {
            Some(cmd) => {
                if !push(&events, Ev::Client { cmd, reply }).await {
                    break;
                }
            }
            None => {
                let _ = reply.send(Reply::Text(
                    "ERR expected PUT <k> <v> | GET <k> | STALE <k> | STATS | RECONFIGURE <ids> [LEADERS <ids>] | LEADERS <ids>"
                        .to_owned(),
                ));
            }
        }
        if tx.send(rx).await.is_err() {
            break;
        }
    }
    drop(tx);
    let _ = writer.await;
}

fn seed() -> [u8; 32] {
    let mut s = [0u8; 32];
    getrandom::fill(&mut s).expect("OS randomness");
    s
}

#[tokio::main]
async fn main() {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("kv-tokio: {e}");
            std::process::exit(2);
        }
    };
    let cfg = config(&args);
    let cluster_id = cfg.cluster_id;
    let (disk, records) = Disk::open(&args.data).expect("open data directory");
    let replica = Replica::new(cfg, records, Timestamp(0)).expect("recover replica");

    let mut peers = HashMap::new();
    let mut listen = None;
    for (i, addr) in args.members.iter().chain(&args.peers) {
        if *i == args.id {
            listen = Some(*addr);
            continue;
        }
        let addr = *addr;
        peers.insert(*i, peer_link(args.id, move || dial(addr)));
    }
    let listen = listen.expect("--members or --peers must include --id");
    let (events, rx) = mpsc::sync_channel(65_536);
    let mut node = Node::new(replica, disk, peers, args.snapshot_every, events.clone());
    if let Err(e) = node.restore() {
        eprintln!("kv-tokio: {e}");
        std::process::exit(1);
    }
    sync::thread::spawn(move || node.run(rx));

    let peer_listener = TcpListener::bind(listen).await.expect("bind peer address");
    let client_listener = TcpListener::bind(args.client).await.expect("bind client address");
    eprintln!("kv-tokio {}: peers on {listen}, clients on {}", args.id, args.client);
    if let Some(addr) = args.etcd {
        let service = grpc::KvServer::new(grpc::EtcdKv::new(events.clone(), cluster_id, args.id));
        eprintln!("kv-tokio {}: etcd v3 KV API on {addr}", args.id);
        tokio::spawn(async move {
            if let Err(e) = tonic::transport::Server::builder().add_service(service).serve(addr).await {
                eprintln!("kv-tokio: etcd API on {addr}: {e}");
                std::process::exit(1);
            }
        });
    }
    let peer_events = events.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = peer_listener.accept().await {
            let _ = s.set_nodelay(true);
            tokio::spawn(inbound(s, peer_events.clone()));
        }
    });
    while let Ok((s, _)) = client_listener.accept().await {
        let _ = s.set_nodelay(true);
        let (r, w) = s.into_split();
        tokio::spawn(client(r, w, events.clone()));
    }
}

#[cfg(all(test, not(feature = "shuttle")))]
mod tests {
    use super::*;

    fn args(line: &str) -> Result<Args, String> {
        parse_args(line.split_whitespace().map(str::to_owned))
    }

    #[test]
    fn a_replica_listed_twice_is_rejected() {
        let base = "--id 0 --client 127.0.0.1:1 --data d --members 0=127.0.0.1:2,1=127.0.0.1:3";
        assert!(args(base).is_ok());
        assert!(args(&format!("{base} --peers 2=127.0.0.1:4")).is_ok());
        let twice = args(&format!("{base} --peers 1=127.0.0.1:4")).err().unwrap();
        assert!(twice.contains("replica 1 is listed twice"), "{twice}");
        assert!(args("--id 0 --client 127.0.0.1:1 --data d --members 0=127.0.0.1:2,0=127.0.0.1:3").is_err());
    }

    #[test]
    fn leader_orders_parse_alone_and_with_members() {
        let order = [NodeId(2), NodeId(0), NodeId(1)];
        assert!(matches!(parse_cmd("LEADERS 2,0,1"), Some(Cmd::LeaderOrder(o)) if o == order));
        assert!(matches!(
            parse_cmd("RECONFIGURE 0,1,2 LEADERS 2,0,1"),
            Some(Cmd::Reconfigure(m, o)) if m == [NodeId(0), NodeId(1), NodeId(2)] && o == order
        ));
        assert!(parse_cmd("LEADERS 2,x").is_none());
    }

    #[test]
    fn wan_timers_reach_the_replica_config() {
        let base = "--id 0 --client 127.0.0.1:1 --data d --members 0=127.0.0.1:2";
        let wan = config(&args(&format!("{base} --retransmit-ms 400 --hedge-max-ms 3000")).unwrap());
        assert_eq!((wan.retransmit_interval, wan.hedge_max), (400 * MILLI, 3_000 * MILLI));
        let pinned = config(&args(&format!("{base},1=127.0.0.1:3 --leader-order 1,0")).unwrap());
        assert_eq!(pinned.leader_order, vec![NodeId(1), NodeId(0)], "the leader stays pinned");
        let plain = config(&args(base).unwrap());
        let default = Config::new([0; 16], NodeId(0), vec![NodeId(0)], [0; 32]);
        assert_eq!(
            (plain.retransmit_interval, plain.hedge_max),
            (default.retransmit_interval, default.hedge_max)
        );
        let zero = config(&args(&format!("{base} --hedge-max-ms 0")).unwrap());
        assert!(!wan.placement && config(&args(&format!("{base} --placement on")).unwrap()).placement);
        assert!(args(&format!("{base} --placement yes")).is_err());
        let periods = config(&args(&format!("{base} --placement-periods 30000,120000")).unwrap());
        assert_eq!((periods.placement_sustain, periods.placement_hold), (30_000 * MILLI, 120_000 * MILLI));
        assert!(args(&format!("{base} --placement-periods 30000")).is_err());
        assert!(Replica::new(zero, [], Timestamp(0)).is_err(), "the replica's own validation refuses it");
        let bad = args(&format!("{base} --retransmit-ms soon")).err().unwrap();
        assert!(bad.starts_with("--retransmit-ms"), "{bad}");
    }

    /// A client connection served by `client`, with the node side of its events.
    async fn connect() -> (TcpStream, mpsc::Receiver<Ev>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (events, rx) = mpsc::sync_channel(4 * MAX_PIPELINE);
        tokio::spawn(async move {
            let (r, w) = listener.accept().await.unwrap().0.into_split();
            client(r, w, events).await
        });
        (TcpStream::connect(addr).await.unwrap(), rx)
    }

    #[tokio::test]
    async fn an_overlong_line_is_refused() {
        let (s, _events) = connect().await;
        let (r, mut w) = s.into_split();
        // More than the server reads before giving up: it must not reset the connection.
        tokio::spawn(async move { w.write_all(&vec![b'x'; 2 * MAX_LINE]).await });
        let mut reply = String::new();
        BufReader::new(r).read_to_string(&mut reply).await.unwrap();
        assert_eq!(reply, format!("ERR line longer than {MAX_LINE} bytes\n"), "then the connection closes");
    }

    #[tokio::test]
    async fn a_client_that_never_reads_cannot_queue_unbounded_requests() {
        let (s, events) = connect().await;
        let (_r, mut w) = s.into_split();
        // The node never answers: every request stays outstanding.
        tokio::spawn(async move {
            for _ in 0..3 * MAX_PIPELINE {
                if w.write_all(b"GET k\n").await.is_err() {
                    return;
                }
            }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let held: Vec<Ev> = events.try_iter().collect();
        assert!(held.len() <= MAX_PIPELINE + 2, "{} requests taken in", held.len());
        assert!(held.len() >= MAX_PIPELINE, "{} requests taken in", held.len());
    }

    #[tokio::test]
    async fn a_peer_connection_that_stops_draining_is_redialed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (tx, rx) = tmpsc::channel(4096);
        let addr = listener.local_addr().unwrap();
        tokio::spawn(outbound(0, move || dial(addr), rx, false));
        // The first connection is accepted and never read, like a link that drops packets.
        let (_stuck, _) = listener.accept().await.unwrap();
        tokio::spawn(async move { while tx.send(vec![0; 64 << 10]).await.is_ok() {} });
        let redial = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await;
        assert!(redial.is_ok(), "the sender kept waiting on a connection that drains nothing");
    }

    #[tokio::test]
    async fn a_large_frame_to_a_slow_but_draining_peer_is_not_cut_off() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (tx, rx) = tmpsc::channel(4);
        let addr = listener.local_addr().unwrap();
        tokio::spawn(outbound(0, move || dial(addr), rx, true));
        let (s, _) = listener.accept().await.unwrap();
        const LEN: usize = 16 << 20; // a large snapshot
        tx.send(vec![7; LEN]).await.unwrap();
        // About 3 MiB/s: the frame takes far longer than `STALL`, but bytes keep moving.
        let mut s = BufReader::new(s);
        let mut got = 0;
        let mut buf = vec![0; CHUNK];
        let started = std::time::Instant::now();
        while got < 4 + 4 + LEN {
            let n = s.read(&mut buf).await.unwrap();
            assert!(n > 0, "the connection was given up after {got} bytes");
            got += n;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(started.elapsed() > STALL, "the link was not slow enough to test anything");
    }
}

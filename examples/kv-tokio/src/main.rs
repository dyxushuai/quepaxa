//! A replicated key-value store over QuePaxa: the reference runtime for `quepaxa`.
//!
//! ```text
//! kv-tokio --id 0 --members 0=127.0.0.1:7100,1=127.0.0.1:7101,2=127.0.0.1:7102 \
//!          --client 127.0.0.1:7200 --data /tmp/kv0 [--snapshot-every 1000] [--leader-order 2,0,1] \
//!          [--peers 3=127.0.0.1:7103] [--etcd 127.0.0.1:2379] [--retransmit-ms 400] [--hedge-max-ms 3000]
//! ```
//!
//! `--retransmit-ms` and `--hedge-max-ms` set the replica's `retransmit_interval` and
//! `hedge_max`; raise them above the longest round trip and commit path on a WAN.
//!
//! `--members` are the configured members; `--peers` are replicas that may join later (a
//! joining replica runs with the same `--members` and finds its own address in `--peers`).
//!
//! Clients speak lines: `PUT <key> <value>` → `OK`, `GET <key>` (linearizable, through a
//! read barrier) → `VALUE <v>` | `NONE`, `STALE <key>` (local, possibly stale), `STATS` →
//! `STATS <counters>`, `RECONFIGURE <ids> [LEADERS <ids>]` (comma-separated) → `OK` once
//! the change is decided. Requests on one connection may be pipelined; replies come back in
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

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::time::Duration;

use bytes::Bytes;
use quepaxa::{Config, NodeId, Replica, Timestamp};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot::error::TryRecvError;
use tokio::sync::{mpsc as tmpsc, oneshot};

/// The event loop and the snapshot thread allocate and free heavily at once; with the system
/// allocator, cross-thread frees made compaction take 2-8 ms instead of under 2 ms.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use client::{Cmd, Reply};
use node::{Ev, MAX_FRAME, Node, PeerFrame};
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
}

const MILLI: u64 = 1_000_000;

fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let (mut id, mut members, mut client, mut data, mut every) = (None, Vec::new(), None, None, 10_000);
    let (mut leader_order, mut peers, mut etcd) = (Vec::new(), Vec::new(), None);
    let (mut retransmit_ms, mut hedge_max_ms) = (None, None);
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
    })
}

/// The replica's configuration: what the flags set, `Config::new`'s defaults otherwise.
fn config(args: &Args) -> Config {
    let members: Vec<NodeId> = args.members.iter().map(|(i, _)| NodeId(*i)).collect();
    let mut cluster_id = [0u8; 16];
    cluster_id[..8].copy_from_slice(b"kv-tokio");
    let mut cfg = Config::new(cluster_id, NodeId(args.id), members, seed());
    cfg.leader_order = args.leader_order.clone();
    if let Some(ms) = args.retransmit_ms {
        cfg.retransmit_interval = ms.saturating_mul(MILLI);
    }
    if let Some(ms) = args.hedge_max_ms {
        cfg.hedge_max = ms.saturating_mul(MILLI);
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

async fn write_frame(s: &mut (impl AsyncWrite + Unpin), frame: &[u8]) -> std::io::Result<()> {
    s.write_u32_le(frame.len() as u32).await?;
    s.write_all(frame).await
}

/// Keeps a connection to `peer`, reconnecting forever with exponential backoff (reset once
/// a connection carries a frame); lost frames are retransmitted by the protocol.
async fn outbound(me: u32, addr: SocketAddr, mut rx: tmpsc::Receiver<Vec<u8>>) {
    const MIN: Duration = Duration::from_millis(100);
    let mut backoff = MIN;
    loop {
        if let Ok(s) = TcpStream::connect(addr).await {
            let _ = s.set_nodelay(true);
            // Frames queued while the previous batch was written go out in one syscall.
            let mut w = BufWriter::with_capacity(64 << 10, s);
            let hello = write_frame(&mut w, &me.to_le_bytes()).await.is_ok() && w.flush().await.is_ok();
            while hello && let Some(frame) = rx.recv().await {
                let mut ok = write_frame(&mut w, &frame).await.is_ok();
                while ok && let Ok(frame) = rx.try_recv() {
                    ok = write_frame(&mut w, &frame).await.is_ok();
                }
                if !ok || w.flush().await.is_err() {
                    break;
                }
                backoff = MIN;
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

async fn inbound(s: TcpStream, events: SyncSender<Ev>) {
    let _ = s.set_nodelay(true);
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
async fn client(s: TcpStream, events: SyncSender<Ev>) {
    let _ = s.set_nodelay(true);
    let (r, w) = s.into_split();
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
                    "ERR expected PUT <k> <v> | GET <k> | STALE <k> | STATS | RECONFIGURE <ids> [LEADERS <ids>]"
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
        let (tx, rx) = tmpsc::channel(4096);
        peers.insert(*i, tx);
        tokio::spawn(outbound(args.id, *addr, rx));
    }
    let listen = listen.expect("--members or --peers must include --id");
    let (events, rx) = mpsc::sync_channel(65_536);
    let mut node = Node::new(replica, disk, peers, args.snapshot_every, events.clone());
    if let Err(e) = node.restore() {
        eprintln!("kv-tokio: {e}");
        std::process::exit(1);
    }
    std::thread::spawn(move || node.run(rx));

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
            tokio::spawn(inbound(s, peer_events.clone()));
        }
    });
    while let Ok((s, _)) = client_listener.accept().await {
        tokio::spawn(client(s, events.clone()));
    }
}

#[cfg(test)]
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
    fn wan_timers_reach_the_replica_config() {
        let base = "--id 0 --client 127.0.0.1:1 --data d --members 0=127.0.0.1:2";
        let wan = config(&args(&format!("{base} --retransmit-ms 400 --hedge-max-ms 3000")).unwrap());
        assert_eq!((wan.retransmit_interval, wan.hedge_max), (400 * MILLI, 3_000 * MILLI));
        let plain = config(&args(base).unwrap());
        let default = Config::new([0; 16], NodeId(0), vec![NodeId(0)], [0; 32]);
        assert_eq!(
            (plain.retransmit_interval, plain.hedge_max),
            (default.retransmit_interval, default.hedge_max)
        );
        let zero = config(&args(&format!("{base} --hedge-max-ms 0")).unwrap());
        assert!(Replica::new(zero, [], Timestamp(0)).is_err(), "the replica's own validation refuses it");
        let bad = args(&format!("{base} --retransmit-ms soon")).err().unwrap();
        assert!(bad.starts_with("--retransmit-ms"), "{bad}");
    }

    /// A client connection served by `client`, with the node side of its events.
    async fn connect() -> (TcpStream, mpsc::Receiver<Ev>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (events, rx) = mpsc::sync_channel(4 * MAX_PIPELINE);
        tokio::spawn(async move { client(listener.accept().await.unwrap().0, events).await });
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
}

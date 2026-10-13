//! A replicated key-value store over QuePaxa: the reference runtime for `quepaxa`.
//!
//! ```text
//! kv-tokio --id 0 --members 0=127.0.0.1:7100,1=127.0.0.1:7101,2=127.0.0.1:7102 \
//!          --client 127.0.0.1:7200 --data /tmp/kv0 [--snapshot-every 1000]
//! ```
//!
//! Clients speak lines: `PUT <key> <value>` → `OK`, `GET <key>` (linearizable, through a
//! read barrier) → `VALUE <v>` | `NONE`, `STALE <key>` (local, possibly stale), `STATS` → `STATS <counters>`. Requests
//! on one connection may be pipelined; replies come back in order.
//!
//! Trusted networks only: peer connections are not authenticated or encrypted, and the
//! peer id is whatever the connecting side claims. Put replicas on a private network or
//! behind mTLS (e.g. a mesh sidecar) before exposing them.

mod kv;
mod node;
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

use node::{Cmd, Ev, MAX_FRAME, Node, PeerFrame};
use storage::Disk;

struct Args {
    id: u32,
    members: Vec<(u32, SocketAddr)>,
    client: SocketAddr,
    data: PathBuf,
    snapshot_every: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let (mut id, mut members, mut client, mut data, mut every) = (None, Vec::new(), None, None, 10_000);
    while let Some(flag) = it.next() {
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--id" => id = Some(value.parse().map_err(|e| format!("--id: {e}"))?),
            "--members" => {
                for m in value.split(',') {
                    let (i, a) = m.split_once('=').ok_or("--members: expected id=addr")?;
                    members.push((
                        i.parse().map_err(|e| format!("--members: {e}"))?,
                        a.parse().map_err(|e| format!("--members: {e}"))?,
                    ));
                }
            }
            "--client" => client = Some(value.parse().map_err(|e| format!("--client: {e}"))?),
            "--data" => data = Some(PathBuf::from(value)),
            "--snapshot-every" => every = value.parse().map_err(|e| format!("--snapshot-every: {e}"))?,
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(Args {
        id: id.ok_or("--id is required")?,
        client: client.ok_or("--client is required")?,
        data: data.ok_or("--data is required")?,
        members,
        snapshot_every: every,
    })
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

/// Keeps a connection to `peer`, reconnecting forever; lost frames are retransmitted by
/// the protocol.
async fn outbound(me: u32, addr: SocketAddr, mut rx: tmpsc::Receiver<Vec<u8>>) {
    loop {
        let Ok(s) = TcpStream::connect(addr).await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let _ = s.set_nodelay(true);
        // Frames queued while the previous batch was written go out in one syscall.
        let mut w = BufWriter::with_capacity(64 << 10, s);
        if write_frame(&mut w, &me.to_le_bytes()).await.is_err() || w.flush().await.is_err() {
            continue;
        }
        while let Some(frame) = rx.recv().await {
            let mut ok = write_frame(&mut w, &frame).await.is_ok();
            while ok && let Ok(frame) = rx.try_recv() {
                ok = write_frame(&mut w, &frame).await.is_ok();
            }
            if !ok || w.flush().await.is_err() {
                break;
            }
        }
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
    let words: Vec<&str> = line.split_whitespace().collect();
    match words[..] {
        ["PUT", k, v] => Some(Cmd::Put(k.to_owned(), v.to_owned())),
        ["GET", k] => Some(Cmd::Get(k.to_owned())),
        ["STALE", k] => Some(Cmd::Stale(k.to_owned())),
        ["STATS"] => Some(Cmd::Stats),
        _ => None,
    }
}

/// Pipelined client connection: requests go to the node as they arrive, replies are
/// written in request order.
async fn client(s: TcpStream, events: SyncSender<Ev>) {
    let _ = s.set_nodelay(true);
    let (r, w) = s.into_split();
    let (tx, mut replies) = tmpsc::unbounded_channel::<oneshot::Receiver<String>>();
    let writer = tokio::spawn(async move {
        // Replies already available are written together; the buffer is flushed before
        // waiting on one that is not.
        let mut w = BufWriter::new(w);
        while let Some(mut rx) = replies.recv().await {
            let line = match rx.try_recv() {
                Ok(line) => line,
                Err(TryRecvError::Empty) => {
                    if w.flush().await.is_err() {
                        return;
                    }
                    rx.await.unwrap_or_else(|_| "ERR node stopped".to_owned())
                }
                Err(TryRecvError::Closed) => "ERR node stopped".to_owned(),
            };
            let written = w.write_all(line.as_bytes()).await.and(w.write_all(b"\n").await);
            if written.is_err() || (replies.is_empty() && w.flush().await.is_err()) {
                return;
            }
        }
    });
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let (reply, rx) = oneshot::channel();
        match parse_cmd(&line) {
            Some(cmd) => {
                if !push(&events, Ev::Client { cmd, reply }).await {
                    break;
                }
            }
            None => {
                let _ = reply.send("ERR expected PUT <k> <v> | GET <k> | STALE <k> | STATS".to_owned());
            }
        }
        if tx.send(rx).is_err() {
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
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("kv-tokio: {e}");
            std::process::exit(2);
        }
    };
    let members: Vec<NodeId> = args.members.iter().map(|(i, _)| NodeId(*i)).collect();
    let mut cluster_id = [0u8; 16];
    cluster_id[..8].copy_from_slice(b"kv-tokio");
    let cfg = Config::new(cluster_id, NodeId(args.id), members, seed());
    let (disk, records) = Disk::open(&args.data).expect("open data directory");
    let replica = Replica::new(cfg, records, Timestamp(0)).expect("recover replica");

    let mut peers = HashMap::new();
    let mut listen = None;
    for (i, addr) in &args.members {
        if *i == args.id {
            listen = Some(*addr);
            continue;
        }
        let (tx, rx) = tmpsc::channel(4096);
        peers.insert(*i, tx);
        tokio::spawn(outbound(args.id, *addr, rx));
    }
    let listen = listen.expect("--members must include --id");
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

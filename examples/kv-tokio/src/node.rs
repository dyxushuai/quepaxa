//! The replica event loop: one thread owns the `Replica`, the disk and the KV state, and
//! executes actions in order. Persists issued while handling a burst of events share one
//! fsync (group commit); their gated messages leave only after it.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use quepaxa::{Action, Input, PersistId, ProposalId, Replica, Slot, Timestamp};
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

use crate::storage::{Disk, Snapshot, decode_snapshot, encode_snapshot};

/// Largest frame on a peer link; snapshots larger than this are not taken (see `maybe_snapshot`).
pub const MAX_FRAME: usize = 64 << 20;

/// A frame between replicas: `[kind u8][payload]`.
pub enum PeerFrame {
    Msg(Bytes),
    SnapshotRequest,
    Snapshot(Bytes),
}

impl PeerFrame {
    pub fn encode(&self) -> Vec<u8> {
        let (kind, body): (u8, &[u8]) = match self {
            PeerFrame::Msg(b) => (0, b),
            PeerFrame::SnapshotRequest => (1, &[]),
            PeerFrame::Snapshot(b) => (2, b),
        };
        let mut v = Vec::with_capacity(1 + body.len());
        v.push(kind);
        v.extend_from_slice(body);
        v
    }

    pub fn decode(b: Bytes) -> Option<Self> {
        match b.first()? {
            0 => Some(PeerFrame::Msg(b.slice(1..))),
            1 => Some(PeerFrame::SnapshotRequest),
            2 => Some(PeerFrame::Snapshot(b.slice(1..))),
            _ => None,
        }
    }
}

pub enum Cmd {
    Put(String, String),
    Get(String),
    Stale(String),
}

pub enum Ev {
    Peer { from: u32, frame: PeerFrame },
    Client { cmd: Cmd, reply: oneshot::Sender<String> },
}

struct PendingPersist {
    id: PersistId,
    records: Vec<Bytes>,
    then: Vec<quepaxa::Outbound>,
    replace: bool,
}

/// Committed operations are `[key len u32][key][value]`, so keys may contain any byte.
fn encode_put(k: &str, v: &str) -> Bytes {
    let mut b = Vec::with_capacity(4 + k.len() + v.len());
    b.extend_from_slice(&(k.len() as u32).to_le_bytes());
    b.extend_from_slice(k.as_bytes());
    b.extend_from_slice(v.as_bytes());
    Bytes::from(b)
}

fn decode_put(b: &[u8]) -> Option<(String, String)> {
    let klen = u32::from_le_bytes(b.get(..4)?.try_into().ok()?) as usize;
    let k = std::str::from_utf8(b.get(4..4usize.checked_add(klen)?)?).ok()?;
    let v = std::str::from_utf8(&b[4 + klen..]).ok()?;
    Some((k.to_owned(), v.to_owned()))
}

pub struct Node {
    replica: Replica,
    disk: Disk,
    kv: BTreeMap<String, String>,
    /// Next slot to apply.
    applied: u64,
    snapshot_every: u64,
    last_snapshot: u64,
    own_snapshot: Option<Bytes>,
    /// The peer a snapshot was requested from; snapshots from anyone else are ignored.
    snapshot_from: Option<u32>,
    peers: HashMap<u32, Sender<Vec<u8>>>,
    start: Instant,
    writes: HashMap<ProposalId, oneshot::Sender<String>>,
    reads: HashMap<ProposalId, (String, oneshot::Sender<String>)>,
    persists: Vec<PendingPersist>,
}

/// How many queued events are handled before their persists are flushed together.
const GROUP: usize = 512;

impl Node {
    pub fn new(replica: Replica, disk: Disk, peers: HashMap<u32, Sender<Vec<u8>>>, every: u64) -> Self {
        Self {
            replica,
            disk,
            kv: BTreeMap::new(),
            applied: 0,
            snapshot_every: every,
            last_snapshot: 0,
            own_snapshot: None,
            snapshot_from: None,
            peers,
            start: Instant::now(),
            writes: HashMap::new(),
            reads: HashMap::new(),
            persists: Vec::new(),
        }
    }

    fn now(&self) -> Timestamp {
        Timestamp(self.start.elapsed().as_nanos() as u64)
    }

    /// Restores the application snapshot taken before a restart, if any. A snapshot the
    /// replica rejects is a corrupted data directory: refuse to start rather than serve a
    /// state inconsistent with the log.
    pub fn restore(&mut self) -> Result<(), String> {
        let bytes = self.disk.load_snapshot().map_err(|e| format!("read snapshot: {e}"))?;
        let Some(bytes) = bytes else { return Ok(()) };
        let snap = decode_snapshot(&bytes).ok_or("snapshot.bin is corrupt")?;
        self.kv = snap.kv;
        self.applied = snap.through + 1;
        self.last_snapshot = self.applied;
        self.drain();
        let now = self.now();
        // The application snapshot may be newer than the replica's last durable checkpoint.
        self.replica
            .install_snapshot(now, Slot(snap.through), snap.core)
            .map_err(|e| format!("snapshot.bin does not match the log: {e}"))?;
        self.own_snapshot = Some(bytes);
        self.drain();
        Ok(())
    }

    pub fn run(mut self, rx: Receiver<Ev>) {
        self.drain();
        self.flush();
        loop {
            // Due timers first: a steady stream of events must not starve retransmission,
            // hedging or catch-up.
            if self.replica.next_deadline().is_some_and(|t| t <= self.now()) {
                let now = self.now();
                let _ = self.replica.handle_timeout(now);
                self.drain();
                self.flush();
            }
            let wait = self.replica.next_deadline().map_or(Duration::from_secs(1), |Timestamp(t)| {
                Duration::from_nanos(t.saturating_sub(self.now().0))
            });
            match rx.recv_timeout(wait) {
                Ok(ev) => {
                    self.on_event(ev);
                    for ev in rx.try_iter().take(GROUP) {
                        self.on_event(ev);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            self.flush();
            self.maybe_snapshot();
        }
    }

    fn on_event(&mut self, ev: Ev) {
        let now = self.now();
        match ev {
            Ev::Peer { frame: PeerFrame::Msg(b), .. } => {
                let _ = self.replica.handle(now, Input::Message(b));
            }
            Ev::Peer { from, frame: PeerFrame::SnapshotRequest } => {
                if let Some(s) = self.own_snapshot.clone() {
                    self.send(from, PeerFrame::Snapshot(s));
                }
            }
            Ev::Peer { from, frame: PeerFrame::Snapshot(bytes) } => {
                if self.snapshot_from == Some(from) {
                    self.install(bytes);
                }
            }
            Ev::Client { cmd, reply } => self.on_client(now, cmd, reply),
        }
        self.drain();
    }

    fn on_client(&mut self, now: Timestamp, cmd: Cmd, reply: oneshot::Sender<String>) {
        match cmd {
            Cmd::Put(k, v) => match self.replica.propose(now, encode_put(&k, &v)) {
                Ok(id) => {
                    self.writes.insert(id, reply);
                }
                Err(e) => {
                    let _ = reply.send(format!("ERR {e}"));
                }
            },
            Cmd::Get(k) => match self.replica.read_barrier(now) {
                Ok(id) => {
                    self.reads.insert(id, (k, reply));
                }
                Err(e) => {
                    let _ = reply.send(format!("ERR {e}"));
                }
            },
            Cmd::Stale(k) => {
                let _ = reply.send(self.value(&k));
            }
        }
    }

    fn value(&self, k: &str) -> String {
        self.kv.get(k).map_or_else(|| "NONE".to_owned(), |v| format!("VALUE {v}"))
    }

    /// Adopts a peer's snapshot. The replica validates it first (its checkpoint persist is
    /// only queued); the application snapshot is then saved, and only if that succeeds may
    /// the checkpoint compact the log.
    fn install(&mut self, bytes: Bytes) {
        let Some(snap) = decode_snapshot(&bytes) else { return };
        let now = self.now();
        let through = snap.through;
        if self.replica.install_snapshot(now, Slot(through), snap.core.clone()) != Ok(true) {
            return;
        }
        // Adopt the state before draining: commits after `through` apply on top of it.
        self.kv = snap.kv.clone();
        self.applied = through + 1;
        self.last_snapshot = self.applied;
        self.snapshot_from = None;
        self.drain();
        if let Err(e) = self.disk.save_snapshot(&snap) {
            eprintln!("kv-tokio: cannot save the received snapshot ({e}); fencing this replica");
            if let Some(last) = self.persists.last().map(|p| p.id) {
                self.persists.clear();
                let _ = self.replica.handle(now, Input::StorageFailed(last));
            }
            return;
        }
        self.own_snapshot = Some(bytes);
    }

    /// Takes every pending action; persists wait for `flush`.
    fn drain(&mut self) {
        let mut actions = Vec::new();
        self.replica.poll_actions(&mut actions);
        for a in actions {
            match a {
                Action::Send(o) => self.send(o.to.0, PeerFrame::Msg(o.bytes)),
                Action::Persist { id, records, then, replace } => {
                    self.persists.push(PendingPersist { id, records, then, replace })
                }
                Action::Commit { slot, entries } => {
                    if slot.0 >= self.applied {
                        for (k, v) in entries.iter().filter_map(|e| decode_put(&e.payload)) {
                            self.kv.insert(k, v);
                        }
                        self.applied = slot.0 + 1;
                    }
                }
                Action::Committed { id, .. } => {
                    if let Some(r) = self.writes.remove(&id) {
                        let _ = r.send("OK".to_owned());
                    }
                }
                Action::ReadReady { id, .. } => {
                    if let Some((k, r)) = self.reads.remove(&id) {
                        let _ = r.send(self.value(&k));
                    }
                }
                Action::SnapshotNeeded { peer, .. } => {
                    self.snapshot_from = Some(peer.0);
                    self.send(peer.0, PeerFrame::SnapshotRequest);
                }
                Action::Dropped { id, .. } => {
                    if let Some(r) = self.writes.remove(&id) {
                        let _ = r.send("ERR outcome unknown (replica fenced)".to_owned());
                    }
                    if let Some((_, r)) = self.reads.remove(&id) {
                        let _ = r.send("ERR replica fenced".to_owned());
                    }
                }
                _ => {}
            }
        }
    }

    /// Bounded per-peer queues: when a peer is unreachable, frames are dropped and the
    /// protocol retransmits what still matters.
    fn send(&self, to: u32, frame: PeerFrame) {
        if let Some(p) = self.peers.get(&to) {
            let _ = p.try_send(frame.encode());
        }
    }

    /// Makes pending persists durable with one fsync, releases their gated messages, and
    /// acknowledges them; repeats while acknowledgements produce new writes.
    fn flush(&mut self) {
        while !self.persists.is_empty() {
            let batch = std::mem::take(&mut self.persists);
            let mut appends: Vec<Bytes> = Vec::new();
            let mut last = None;
            let mut gated = Vec::new();
            let mut ok = true;
            for p in batch {
                if p.replace {
                    appends.clear();
                    // After a failure nothing more is written: later records assume it.
                    ok = ok && self.disk.replace(&p.records).is_ok();
                } else {
                    appends.extend(p.records);
                }
                gated.extend(p.then);
                last = Some(p.id);
            }
            ok = ok && (appends.is_empty() || self.disk.append(&appends).is_ok());
            let Some(id) = last else { return };
            let now = self.now();
            if !ok {
                eprintln!("kv-tokio: storage failed; fencing this replica");
                let _ = self.replica.handle(now, Input::StorageFailed(id));
                self.drain();
                return;
            }
            for o in gated {
                self.send(o.to.0, PeerFrame::Msg(o.bytes));
            }
            let _ = self.replica.handle(now, Input::Persisted(id));
            self.drain();
        }
    }

    /// Snapshots the KV state durably, then lets the replica compact its log. Snapshots
    /// too large for one peer frame are not taken (the log keeps growing instead), so a
    /// lagging replica can always fetch the latest snapshot.
    fn maybe_snapshot(&mut self) {
        if self.applied == 0 || self.applied < self.last_snapshot + self.snapshot_every {
            return;
        }
        let through = self.applied - 1;
        let snap = Snapshot { through, core: self.replica.core_state(), kv: self.kv.clone() };
        let bytes = Bytes::from(encode_snapshot(&snap));
        if bytes.len() + 1 > MAX_FRAME {
            eprintln!(
                "kv-tokio: snapshot of {} bytes exceeds the peer frame limit; not compacting",
                bytes.len()
            );
            self.last_snapshot = self.applied;
            return;
        }
        if let Err(e) = self.disk.save_snapshot(&snap) {
            eprintln!("kv-tokio: cannot save snapshot ({e}); not compacting");
            return;
        }
        let now = self.now();
        if self.replica.compact(now, Slot(through), snap.core).is_ok() {
            self.own_snapshot = Some(bytes);
            self.last_snapshot = self.applied;
        }
        self.drain();
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_encoding_keeps_any_key() {
        let b = encode_put("a=b c", "v=1");
        assert_eq!(decode_put(&b), Some(("a=b c".to_owned(), "v=1".to_owned())));
        assert_eq!(decode_put(&[9, 0, 0, 0, b'x']), None);
    }
}

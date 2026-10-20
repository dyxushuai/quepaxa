//! The replica event loop: one thread owns the `Replica`, the disk and the KV state, and
//! executes actions in order. Persists issued while handling a burst of events share one
//! fsync (group commit); their gated messages leave only after it.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use bytes::Bytes;
use quepaxa::{Action, Input, NodeId, PersistId, ProposalId, Replica, Slot, Timestamp};
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

use crate::kv::Kv;
use crate::storage::{Disk, SnapshotWriter, decode_snapshot, encode_snapshot};

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
    /// The replica's counters, on one line.
    Stats,
    /// New members, preferred leaders first (empty: by id).
    Reconfigure(Vec<NodeId>, Vec<NodeId>),
}

pub enum Ev {
    Peer {
        from: u32,
        frame: PeerFrame,
    },
    Client {
        cmd: Cmd,
        reply: oneshot::Sender<String>,
    },
    /// A background snapshot write finished: `Ok(false)` if a newer snapshot won.
    SnapshotSaved {
        through: u64,
        core: Bytes,
        bytes: Bytes,
        saved: Result<bool, String>,
    },
    /// The writer thread made persists up to `id` durable (or failed to).
    Persisted {
        id: PersistId,
        then: Vec<quepaxa::Outbound>,
        ok: bool,
    },
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
    /// Taken by the writer thread when the node runs.
    disk: Option<Disk>,
    snapshots: SnapshotWriter,
    /// Persists for the writer thread, in order.
    jobs: Option<std::sync::mpsc::Sender<Vec<PendingPersist>>>,
    kv: Kv,
    /// Next slot to apply.
    applied: u64,
    snapshot_every: u64,
    last_snapshot: u64,
    /// The newest snapshot saved here (its `through` and encoding), served to peers.
    own_snapshot: Option<(u64, Bytes)>,
    /// A snapshot is being written in the background.
    snapshotting: bool,
    /// For background work to report back to the event loop.
    events: SyncSender<Ev>,
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
    pub fn new(
        replica: Replica,
        disk: Disk,
        peers: HashMap<u32, Sender<Vec<u8>>>,
        every: u64,
        events: SyncSender<Ev>,
    ) -> Self {
        Self {
            replica,
            snapshots: disk.snapshots(),
            disk: Some(disk),
            jobs: None,
            kv: Kv::default(),
            applied: 0,
            snapshot_every: every,
            last_snapshot: 0,
            own_snapshot: None,
            snapshotting: false,
            events,
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
        let disk = self.disk.as_ref().ok_or("restore before run")?;
        let bytes = disk.load_snapshot().map_err(|e| format!("read snapshot: {e}"))?;
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
        self.own_snapshot = Some((snap.through, bytes));
        self.drain();
        Ok(())
    }

    pub fn run(mut self, rx: Receiver<Ev>) {
        if let Some(disk) = self.disk.take() {
            let (jobs, queue) = std::sync::mpsc::channel();
            let events = self.events.clone();
            std::thread::spawn(move || write_persists(disk, &queue, &events));
            self.jobs = Some(jobs);
        }
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
                if let Some((_, s)) = self.own_snapshot.clone() {
                    self.send(from, PeerFrame::Snapshot(s));
                }
            }
            Ev::Peer { from, frame: PeerFrame::Snapshot(bytes) } => {
                if self.snapshot_from == Some(from) {
                    self.install(bytes);
                }
            }
            Ev::Client { cmd, reply } => self.on_client(now, cmd, reply),
            Ev::Persisted { id, then, ok } => {
                if !ok {
                    eprintln!("kv-tokio: storage failed; fencing this replica");
                    self.persists.clear();
                    self.jobs = None;
                    let _ = self.replica.handle(now, Input::StorageFailed(id));
                } else {
                    for o in then {
                        self.send(o.to.0, PeerFrame::Msg(o.bytes));
                    }
                    let _ = self.replica.handle(now, Input::Persisted(id));
                }
            }
            Ev::SnapshotSaved { through, core, bytes, saved } => {
                self.snapshotting = false;
                match saved {
                    // The application snapshot is durable: the log below it may go.
                    Ok(true) => {
                        // A newer (peer) snapshot may have been saved after this one.
                        let newest = self.own_snapshot.as_ref().is_none_or(|(t, _)| *t < through);
                        if self.replica.compact(now, Slot(through), core).is_ok() && newest {
                            self.own_snapshot = Some((through, bytes));
                        }
                    }
                    Ok(false) => {} // a newer (peer) snapshot was saved meanwhile
                    Err(e) => eprintln!("kv-tokio: cannot save snapshot ({e}); not compacting"),
                }
            }
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
            Cmd::Stats => {
                let _ = reply.send(format!("STATS {:?}", self.replica.metrics()));
            }
            Cmd::Reconfigure(members, leaders) => {
                // A member nobody can reach would be decided all the same, and could cost the
                // new epoch its quorum for good.
                let me = self.replica.id();
                if let Some(m) = members.iter().find(|m| **m != me && !self.peers.contains_key(&m.0)) {
                    let _ = reply.send(format!("ERR unknown replica {} (add it to --peers)", m.0));
                    return;
                }
                match self.replica.propose_reconfiguration(now, members, leaders) {
                    Ok(id) => {
                        self.writes.insert(id, reply);
                    }
                    Err(e) => {
                        let _ = reply.send(format!("ERR {e}"));
                    }
                }
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
        if self.replica.install_snapshot(now, Slot(through), snap.core) != Ok(true) {
            return;
        }
        // Adopt the state before draining: commits after `through` apply on top of it.
        self.kv = snap.kv;
        self.applied = through + 1;
        self.last_snapshot = self.applied;
        self.snapshot_from = None;
        self.drain();
        if let Err(e) = self.snapshots.save(through, &bytes) {
            eprintln!("kv-tokio: cannot save the received snapshot ({e}); fencing this replica");
            if let Some(last) = self.persists.last().map(|p| p.id) {
                self.persists.clear();
                let _ = self.replica.handle(now, Input::StorageFailed(last));
            }
            return;
        }
        self.own_snapshot = Some((through, bytes));
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
                Action::Reconfigured { epoch, from, members } => {
                    eprintln!("kv-tokio: epoch {epoch} from slot {}: members {members:?}", from.0);
                }
                // The new members durably hold everything this replica was needed for, and no
                // change decided before the replica's own read barrier added it back.
                Action::Retired { epoch } => {
                    eprintln!("kv-tokio: not a member of epoch {epoch} and no longer needed; exiting");
                    std::process::exit(0);
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

    /// Hands pending persists to the writer thread; their messages leave, and the replica
    /// hears of them, once they are durable (`Ev::Persisted`). The loop keeps running
    /// meanwhile, so persists issued during one fsync share the next (group commit).
    fn flush(&mut self) {
        if self.persists.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.persists);
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(batch);
        }
    }

    /// Snapshots the KV state durably, then lets the replica compact its log. Snapshots
    /// too large for one peer frame are not taken (the log keeps growing instead), so a
    /// lagging replica can always fetch the latest snapshot.
    /// Encodes a snapshot on the event loop (a consistent cut) and writes it in the
    /// background, so the fsync never stalls consensus; the log is compacted once it is
    /// durable (`Ev::SnapshotSaved`). Only the shard pointers are copied here.
    fn maybe_snapshot(&mut self) {
        if self.snapshotting || self.applied == 0 || self.applied < self.last_snapshot + self.snapshot_every {
            return;
        }
        let through = self.applied - 1;
        let (core, kv) = (self.replica.core_state(), self.kv.clone());
        // The next attempt waits a full interval, also after a failure.
        self.last_snapshot = self.applied;
        self.snapshotting = true;
        let (writer, events) = (self.snapshots.clone(), self.events.clone());
        std::thread::spawn(move || {
            let bytes = Bytes::from(encode_snapshot(through, &core, &kv));
            // Unshare the shards now: until then every write copies the shard it touches.
            drop(kv);
            let saved = if bytes.len() + 1 > MAX_FRAME {
                Err(format!("{} bytes exceed the peer frame limit", bytes.len()))
            } else {
                writer.save(through, &bytes).map_err(|e| e.to_string())
            };
            let _ = events.send(Ev::SnapshotSaved { through, core, bytes, saved });
        });
    }
}

/// The writer thread: makes persists durable in order, each group with one fsync, and
/// reports them; after a failure it writes nothing more (later records assume it).
fn write_persists(
    mut disk: Disk,
    queue: &std::sync::mpsc::Receiver<Vec<PendingPersist>>,
    events: &SyncSender<Ev>,
) {
    while let Ok(mut batch) = queue.recv() {
        while let Ok(more) = queue.try_recv() {
            batch.extend(more);
        }
        let mut appends: Vec<Bytes> = Vec::new();
        let (mut last, mut then, mut ok) = (None, Vec::new(), true);
        for p in batch {
            if p.replace {
                appends.clear();
                ok = ok && disk.replace(&p.records).is_ok();
            } else {
                appends.extend(p.records);
            }
            then.extend(p.then);
            last = Some(p.id);
        }
        ok = ok && (appends.is_empty() || disk.append(&appends).is_ok());
        let Some(id) = last else { continue };
        if events.send(Ev::Persisted { id, then, ok }).is_err() || !ok {
            return;
        }
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

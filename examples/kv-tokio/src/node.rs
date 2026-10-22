//! The replica event loop: one thread owns the `Replica`, the disk and the KV state, and
//! executes actions in order. Persists issued while handling a burst of events share one
//! fsync (group commit); their gated messages leave only after it.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
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
    /// The writer thread has reports in `Written`.
    Written,
}

/// The writer thread made persists up to `id` durable (or failed to).
struct Report {
    id: PersistId,
    then: Vec<quepaxa::Outbound>,
    ok: bool,
}

/// What the writer thread has done, shared with the event loop. Reports pass here rather
/// than through the event queue so the writer never blocks: the event loop may be waiting
/// for it (see `MAX_UNWRITTEN`) and so not making room in that queue.
#[derive(Default)]
struct Written {
    state: Mutex<(u64, Vec<Report>)>,
    advanced: Condvar,
}

impl Written {
    fn lock(&self) -> std::sync::MutexGuard<'_, (u64, Vec<Report>)> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `bytes` of records are now durable (or will never be written), with `report`.
    fn advance(&self, bytes: u64, report: Option<Report>) {
        let mut state = self.lock();
        state.0 = bytes;
        state.1.extend(report);
        self.advanced.notify_all();
    }

    fn wait_for(&self, bytes: u64) {
        let mut state = self.lock();
        while state.0 < bytes {
            state = self.advanced.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn reports(&self) -> Vec<Report> {
        std::mem::take(&mut self.lock().1)
    }
}

/// Record bytes that may be handed to the writer thread and not yet be durable. Past this,
/// the event loop waits for the disk: a slow disk then backs up the event queue, and through
/// it peers and clients, instead of piling records up in memory.
const MAX_UNWRITTEN: u64 = 64 << 20;
/// Pending record bytes past which a group of events is flushed early, so one hand-off
/// cannot overshoot `MAX_UNWRITTEN` by much.
const FLUSH_EARLY: u64 = MAX_UNWRITTEN / 8;

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
    /// Record bytes handed to the writer thread, and how many of them it has written.
    handed: u64,
    written: Arc<Written>,
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
    /// Record bytes in `persists`.
    pending: u64,
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
            handed: 0,
            written: Arc::default(),
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
            pending: 0,
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
            let (events, written) = (self.events.clone(), self.written.clone());
            std::thread::spawn(move || {
                write_persists(disk, &queue, &events, &written);
                // Nothing more will be written: the event loop must not wait for it.
                written.advance(u64::MAX, None);
            });
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
                        if self.pending > FLUSH_EARLY {
                            self.flush();
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            self.on_written();
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
            Ev::Written => {} // taken by `on_written`
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

    /// Takes the writer thread's reports: gated messages leave, and the replica hears of
    /// its persists. Checked on every turn, since a wake-up may not fit in the event queue.
    fn on_written(&mut self) {
        // Each report is drained before the next: the replica refuses input while actions
        // are pending.
        for Report { id, then, ok } in self.written.reports() {
            let now = self.now();
            if !ok {
                eprintln!("kv-tokio: storage failed; fencing this replica");
                self.persists.clear();
                self.pending = 0;
                self.jobs = None;
                let _ = self.replica.handle(now, Input::StorageFailed(id));
                self.drain();
                break;
            }
            for o in then {
                self.send(o.to.0, PeerFrame::Msg(o.bytes));
            }
            let _ = self.replica.handle(now, Input::Persisted(id));
            self.drain();
        }
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
                self.pending = 0;
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
                    self.pending += records.iter().map(|r| r.len() as u64).sum::<u64>();
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
    /// hears of them, once they are durable (`on_written`). The loop keeps running
    /// meanwhile, so persists issued during one fsync share the next (group commit).
    fn flush(&mut self) {
        if self.persists.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.persists);
        let bytes = std::mem::take(&mut self.pending);
        if let Some(jobs) = &self.jobs {
            self.handed += bytes;
            let _ = jobs.send(batch);
            self.written.wait_for(self.handed.saturating_sub(MAX_UNWRITTEN));
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

fn batch_bytes(batch: &[PendingPersist]) -> u64 {
    batch.iter().flat_map(|p| &p.records).map(|r| r.len() as u64).sum()
}

/// The writer thread: makes persists durable in order, each group with one fsync, and
/// reports them; after a failure it writes nothing more (later records assume it).
fn write_persists(
    mut disk: Disk,
    queue: &std::sync::mpsc::Receiver<Vec<PendingPersist>>,
    events: &SyncSender<Ev>,
    written: &Written,
) {
    let mut total = 0;
    while let Ok(mut batch) = queue.recv() {
        while let Ok(more) = queue.try_recv() {
            batch.extend(more);
        }
        total += batch_bytes(&batch);
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
        let Some(id) = last else {
            written.advance(total, None);
            continue;
        };
        written.advance(total, Some(Report { id, then, ok }));
        // A full queue needs no wake-up: the event loop has events to handle and takes
        // the reports with them.
        if matches!(events.try_send(Ev::Written), Err(TrySendError::Disconnected(_))) || !ok {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single-member node whose persists are reported by hand (no writer thread).
    fn node(name: &str) -> Node {
        let dir = std::env::temp_dir().join(format!("kv-tokio-node-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (disk, records) = Disk::open(&dir).unwrap();
        let cfg = quepaxa::Config::new([1; 16], NodeId(0), vec![NodeId(0)], [2; 32]);
        let replica = Replica::new(cfg, records, Timestamp(0)).unwrap();
        let (events, _) = std::sync::mpsc::sync_channel(16);
        let mut n = Node::new(replica, disk, HashMap::new(), u64::MAX, events);
        n.drain();
        n
    }

    #[test]
    fn every_report_reaches_the_replica_even_when_several_arrive_together() {
        let mut n = node("reports");
        let (reply, _) = oneshot::channel();
        n.on_client(n.now(), Cmd::Put("k".to_owned(), "v".to_owned()), reply);
        n.drain();
        let id = n.persists.last().expect("the proposal persists").id;
        // Two reports taken in one turn: the first produces actions, the second is a failure.
        let report = |ok| Report { id, then: Vec::new(), ok };
        n.written.advance(0, Some(report(true)));
        n.written.advance(0, Some(report(false)));
        n.on_written();
        assert_eq!(
            n.replica.propose(n.now(), Bytes::from_static(b"x")),
            Err(quepaxa::Error::Fenced),
            "the storage failure fenced the replica"
        );
    }

    #[test]
    fn waiting_for_the_disk_ends_once_the_writer_catches_up() {
        let written = Arc::new(Written::default());
        let writer = written.clone();
        let t = std::thread::spawn(move || {
            for bytes in [10, 20, 30] {
                std::thread::sleep(Duration::from_millis(5));
                writer.advance(bytes, None);
            }
        });
        written.wait_for(0);
        written.wait_for(25);
        assert!(written.lock().0 >= 25);
        t.join().unwrap();
        written.advance(u64::MAX, None);
        written.wait_for(u64::MAX - 1);
    }

    #[test]
    fn put_encoding_keeps_any_key() {
        let b = encode_put("a=b c", "v=1");
        assert_eq!(decode_put(&b), Some(("a=b c".to_owned(), "v=1".to_owned())));
        assert_eq!(decode_put(&[9, 0, 0, 0, b'x']), None);
    }
}

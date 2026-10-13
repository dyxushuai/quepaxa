//! The replica event loop: one thread owns the `Replica`, the disk and the KV state, and
//! executes actions in order. Persists issued while handling a burst of events share one
//! fsync (group commit); their gated messages leave only after it.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use quepaxa::{Action, Input, PersistId, ProposalId, Replica, Slot, Timestamp};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::storage::{Disk, Snapshot, decode_snapshot, encode_snapshot};

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

pub struct Node {
    pub replica: Replica,
    pub disk: Disk,
    pub kv: BTreeMap<String, String>,
    /// Next slot to apply.
    pub applied: u64,
    pub snapshot_every: u64,
    pub last_snapshot: u64,
    pub own_snapshot: Option<Bytes>,
    pub peers: HashMap<u32, UnboundedSender<Vec<u8>>>,
    pub start: Instant,
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
        peers: HashMap<u32, UnboundedSender<Vec<u8>>>,
        every: u64,
    ) -> Self {
        Self {
            replica,
            disk,
            kv: BTreeMap::new(),
            applied: 0,
            snapshot_every: every,
            last_snapshot: 0,
            own_snapshot: None,
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

    /// Restores the application snapshot taken before a restart, if any.
    pub fn restore(&mut self) {
        let Ok(Some(bytes)) = self.disk.load_snapshot() else { return };
        let Some(snap) = decode_snapshot(&bytes) else { return };
        self.kv = snap.kv;
        self.applied = snap.through + 1;
        self.last_snapshot = self.applied;
        self.drain();
        let now = self.now();
        // The application snapshot may be newer than the replica's last durable checkpoint.
        if self.replica.install_snapshot(now, Slot(snap.through), snap.core).is_ok() {
            self.own_snapshot = Some(bytes);
        }
        self.drain();
    }

    pub fn run(mut self, rx: Receiver<Ev>) {
        self.drain();
        self.flush();
        loop {
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
                Err(RecvTimeoutError::Timeout) => {
                    let now = self.now();
                    let _ = self.replica.handle_timeout(now);
                    self.drain();
                }
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
                if let (Some(s), Some(peer)) = (&self.own_snapshot, self.peers.get(&from)) {
                    let _ = peer.send(PeerFrame::Snapshot(s.clone()).encode());
                }
            }
            Ev::Peer { frame: PeerFrame::Snapshot(bytes), .. } => self.install(bytes),
            Ev::Client { cmd, reply } => self.on_client(now, cmd, reply),
        }
        self.drain();
    }

    fn on_client(&mut self, now: Timestamp, cmd: Cmd, reply: oneshot::Sender<String>) {
        match cmd {
            Cmd::Put(k, v) => match self.replica.propose(now, Bytes::from(format!("{k}={v}"))) {
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
                let _ =
                    reply.send(self.kv.get(&k).map_or_else(|| "NONE".to_owned(), |v| format!("VALUE {v}")));
            }
        }
    }

    fn install(&mut self, bytes: Bytes) {
        let Some(snap) = decode_snapshot(&bytes) else { return };
        if snap.through < self.replica.emitted_index() {
            return; // stale in transit: never rewind the application
        }
        let now = self.now();
        let through = snap.through;
        if let Ok(true) = self.replica.install_snapshot(now, Slot(through), snap.core.clone()) {
            self.kv = snap.kv;
            self.applied = through + 1;
            self.last_snapshot = self.applied;
            let _ = self.disk.save_snapshot(&Snapshot { through, core: snap.core, kv: self.kv.clone() });
            self.own_snapshot = Some(bytes);
        }
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
                        for e in entries {
                            let s = String::from_utf8_lossy(&e.payload);
                            if let Some((k, v)) = s.split_once('=') {
                                self.kv.insert(k.to_owned(), v.to_owned());
                            }
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
                        let _ = r.send(
                            self.kv.get(&k).map_or_else(|| "NONE".to_owned(), |v| format!("VALUE {v}")),
                        );
                    }
                }
                Action::SnapshotNeeded { peer, .. } => self.send(peer.0, PeerFrame::SnapshotRequest),
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

    fn send(&self, to: u32, frame: PeerFrame) {
        if let Some(p) = self.peers.get(&to) {
            let _ = p.send(frame.encode());
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
                    ok &= self.disk.replace(&p.records).is_ok();
                } else {
                    appends.extend(p.records);
                }
                gated.extend(p.then);
                last = Some(p.id);
            }
            ok &= appends.is_empty() || self.disk.append(&appends).is_ok();
            let Some(id) = last else { return };
            let now = self.now();
            if !ok {
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

    fn maybe_snapshot(&mut self) {
        if self.applied < self.last_snapshot + self.snapshot_every || self.applied == 0 {
            return;
        }
        let through = self.applied - 1;
        let snap = Snapshot { through, core: self.replica.core_state(), kv: self.kv.clone() };
        if self.disk.save_snapshot(&snap).is_err() {
            return;
        }
        let now = self.now();
        if self.replica.compact(now, Slot(through), snap.core.clone()).is_ok() {
            self.own_snapshot = Some(Bytes::from(encode_snapshot(&snap)));
            self.last_snapshot = self.applied;
        }
        self.drain();
        self.flush();
    }
}

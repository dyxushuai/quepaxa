//! A complete, in-memory driver for a cluster of [`Replica`]s, shared by the examples.
//!
//! It is the smallest correct embedding of the crate, written to be read:
//!
//! - every call is followed by draining [`Replica::poll_actions`];
//! - [`Action::Persist`] records go to a per-replica "disk" (a `Vec<Bytes>`) *before* its
//!   `then` messages are released and [`Input::Persisted`] is reported;
//! - the network is a FIFO queue between replicas (a real one may lose, duplicate and
//!   reorder; the protocol copes, see `crates/quepaxa-sim` for a hostile one);
//! - time only moves when nothing else can happen, to the earliest
//!   [`Replica::next_deadline`];
//! - the "application" is the list of committed payloads, plus the [`Replica::core_state`]
//!   that a snapshot of it must carry.
//!
//! A production embedding replaces the disk with an fsync'd file, the queue with sockets
//! and the virtual clock with a monotonic one; see `examples/kv-tokio` in the repository.

#![allow(dead_code)] // each example uses a different part of the driver

use std::collections::VecDeque;

use bytes::Bytes;
use quepaxa::{Action, Config, Input, NodeId, ProposalId, Replica, Slot, Timestamp};

/// One replica with its durable storage and application state.
pub struct Node {
    /// `None` while the replica is down.
    pub replica: Option<Replica>,
    /// Every durable record, in write order: exactly what `Replica::new` needs on restart.
    pub disk: Vec<Bytes>,
    /// The application: committed payloads in log order (slot by slot).
    pub applied: Vec<Bytes>,
    /// Next slot the application expects; earlier slots re-emitted after a restart are
    /// skipped.
    pub next_slot: u64,
    /// The latest application snapshot: its last slot, the payloads, and the replica's
    /// core state taken right after applying that slot.
    pub snapshot: Option<(u64, Vec<Bytes>, Bytes)>,
    /// Local proposals reported committed, and read barriers reported ready.
    pub committed: Vec<ProposalId>,
    pub ready: Vec<(ProposalId, Slot)>,
    /// Membership changes as reported, in log order.
    pub reconfigured: Vec<(u64, Slot, Vec<NodeId>)>,
}

/// A cluster connected by an in-memory network, with a virtual clock in nanoseconds.
pub struct Cluster {
    pub nodes: Vec<Node>,
    net: VecDeque<(NodeId, Bytes)>,
    pub now: u64,
    configs: Vec<Config>,
}

/// The configuration every example uses: same cluster id and members everywhere, and a
/// seed that differs per replica *and per start* (here: derived from the start count).
pub fn config(id: u32, members: &[u32], start: u8) -> Config {
    let members = members.iter().copied().map(NodeId).collect();
    let mut seed = [0u8; 32];
    seed[0] = id as u8;
    seed[1] = start; // never reuse a seed across restarts
    Config::new(*b"example-cluster!", NodeId(id), members, seed)
}

impl Cluster {
    /// Starts `n` fresh replicas `0..n`, all members.
    pub fn new(n: u32) -> Self {
        let members: Vec<u32> = (0..n).collect();
        let configs: Vec<Config> = members.iter().map(|&i| config(i, &members, 0)).collect();
        Self::from_configs(configs)
    }

    /// Starts one replica per configuration (a joining replica keeps the original members).
    pub fn from_configs(configs: Vec<Config>) -> Self {
        let mut cluster = Self { nodes: Vec::new(), net: VecDeque::new(), now: 0, configs: Vec::new() };
        for cfg in configs {
            cluster.add(cfg);
        }
        cluster
    }

    /// Adds and starts a replica.
    pub fn add(&mut self, cfg: Config) {
        let replica = Replica::new(cfg.clone(), [], Timestamp(self.now)).expect("valid config");
        self.configs.push(cfg);
        self.nodes.push(Node {
            replica: Some(replica),
            disk: Vec::new(),
            applied: Vec::new(),
            next_slot: 0,
            snapshot: None,
            committed: Vec::new(),
            ready: Vec::new(),
            reconfigured: Vec::new(),
        });
        let i = self.nodes.len() - 1;
        self.drain(i);
    }

    pub fn replica(&mut self, i: usize) -> &mut Replica {
        self.nodes[i].replica.as_mut().expect("replica is up")
    }

    /// Proposes `payload` at replica `i`.
    pub fn propose(&mut self, i: usize, payload: &str) -> ProposalId {
        let now = Timestamp(self.now);
        let id = self.replica(i).propose(now, Bytes::copy_from_slice(payload.as_bytes())).expect("accepted");
        self.drain(i);
        id
    }

    /// Requests a linearizable read point at replica `i`.
    pub fn read_barrier(&mut self, i: usize) -> ProposalId {
        let now = Timestamp(self.now);
        let id = self.replica(i).read_barrier(now).expect("accepted");
        self.drain(i);
        id
    }

    /// Proposes new membership at replica `i` (`leaders` first to last; empty: by id).
    pub fn propose_reconfiguration(&mut self, i: usize, members: &[u32], leaders: &[u32]) -> ProposalId {
        let now = Timestamp(self.now);
        let members = members.iter().copied().map(NodeId).collect();
        let leaders = leaders.iter().copied().map(NodeId).collect();
        let id = self.replica(i).propose_reconfiguration(now, members, leaders).expect("no change pending");
        self.drain(i); // like every call: drain before the next one
        id
    }

    /// Crashes replica `i`: its memory is gone, its disk stays.
    pub fn crash(&mut self, i: usize) {
        self.nodes[i].replica = None;
        self.nodes[i].applied.clear();
        self.nodes[i].next_slot = 0;
        self.net.retain(|(to, _)| to.0 as usize != i);
    }

    /// Restarts replica `i` from its disk and its latest application snapshot, with a fresh
    /// seed.
    pub fn restart(&mut self, i: usize, start: u8) {
        // The same configuration, except a fresh seed.
        let mut cfg = self.configs[i].clone();
        cfg.seed[1] = start;
        let mut replica =
            Replica::new(cfg, self.nodes[i].disk.clone(), Timestamp(self.now)).expect("recovers");
        let node = &mut self.nodes[i];
        // The application restores its own snapshot first, then tells the replica about it:
        // the snapshot may be newer than the replica's last durable compaction.
        if let Some((through, payloads, core_state)) = node.snapshot.clone() {
            node.applied = payloads;
            node.next_slot = through + 1;
            let mut actions = Vec::new();
            replica.poll_actions(&mut actions); // drain the startup actions first
            node.replica = Some(replica);
            self.execute(i, actions);
            let now = Timestamp(self.now);
            let installed = self.replica(i).install_snapshot(now, Slot(through), core_state);
            assert!(installed.is_ok(), "own snapshot is valid");
        } else {
            node.replica = Some(replica);
        }
        self.drain(i);
    }

    /// Takes an application snapshot at replica `i` and lets the replica compact its log.
    pub fn snapshot_and_compact(&mut self, i: usize) {
        let node = &mut self.nodes[i];
        let Some(through) = node.next_slot.checked_sub(1) else { return };
        let core_state = node.replica.as_ref().expect("up").core_state();
        // In production: write the snapshot durably (fsync) before calling `compact`.
        node.snapshot = Some((through, node.applied.clone(), core_state.clone()));
        let now = Timestamp(self.now);
        self.replica(i).compact(now, Slot(through), core_state).expect("valid compaction");
        self.drain(i);
    }

    /// Delivers messages and completes writes until nothing is left; then, if `timers`,
    /// advances the clock to the earliest deadline and repeats, up to `limit` nanoseconds.
    pub fn run_for(&mut self, limit: u64) {
        let end = self.now + limit;
        loop {
            while let Some((to, bytes)) = self.net.pop_front() {
                let i = to.0 as usize;
                let now = Timestamp(self.now);
                if let Some(r) = self.nodes.get_mut(i).and_then(|n| n.replica.as_mut()) {
                    r.handle(now, Input::Message(bytes)).expect("drained before");
                    self.drain(i);
                }
            }
            let next = self.nodes.iter().filter_map(|n| n.replica.as_ref()?.next_deadline()).min();
            match next {
                Some(Timestamp(at)) if at <= end => {
                    self.now = self.now.max(at);
                    for i in 0..self.nodes.len() {
                        let now = Timestamp(self.now);
                        let Some(r) = self.nodes[i].replica.as_mut() else { continue };
                        if r.next_deadline().is_some_and(|d| d <= now) {
                            r.handle_timeout(now).expect("drained before");
                            self.drain(i);
                        }
                    }
                }
                _ => {
                    self.now = end;
                    return;
                }
            }
        }
    }

    /// Delivers everything that is ready, without moving the clock.
    pub fn settle(&mut self) {
        self.run_for(0);
    }

    /// Takes replica `i`'s pending actions and carries them out.
    fn drain(&mut self, i: usize) {
        let mut actions = Vec::new();
        if let Some(r) = self.nodes[i].replica.as_mut() {
            r.poll_actions(&mut actions);
        }
        self.execute(i, actions);
    }

    fn execute(&mut self, i: usize, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Persist { id, records, then, replace } => {
                    // 1. Make the records durable (a real disk: append + fsync, or for
                    //    `replace`, write a new file and rename it over the old one).
                    let disk = &mut self.nodes[i].disk;
                    if replace {
                        disk.clear();
                    }
                    disk.extend(records);
                    // 2. Only now may the messages that depend on them leave.
                    self.net.extend(then.into_iter().map(|o| (o.to, o.bytes)));
                    // 3. Report completion; this may produce more actions.
                    let now = Timestamp(self.now);
                    self.replica(i).handle(now, Input::Persisted(id)).expect("issued id");
                    self.drain(i);
                }
                Action::Send(out) => self.net.push_back((out.to, out.bytes)),
                Action::Commit { slot, entries } => {
                    let node = &mut self.nodes[i];
                    // Slots arrive in order; after a restart some are emitted again.
                    if slot.0 >= node.next_slot {
                        node.applied.extend(entries.into_iter().map(|e| e.payload));
                        node.next_slot = slot.0 + 1;
                    }
                }
                Action::Committed { id, .. } => self.nodes[i].committed.push(id),
                Action::ReadReady { id, slot } => self.nodes[i].ready.push((id, slot)),
                Action::Reconfigured { epoch, from, members } => {
                    self.nodes[i].reconfigured.push((epoch, from, members))
                }
                Action::SnapshotNeeded { peer, through } => self.install_from(i, peer.0 as usize, through),
                Action::Dropped { .. } => {} // only after a storage failure
                _ => {}                      // `Action` is non-exhaustive
            }
        }
    }

    /// Ships `peer`'s application snapshot to replica `i` (out of band, e.g. over HTTP).
    fn install_from(&mut self, i: usize, peer: usize, through: Slot) {
        let snapshot = self.nodes[peer].snapshot.clone();
        let Some((snap_through, payloads, core_state)) = snapshot.clone() else { return };
        if snap_through < through.0 {
            return;
        }
        let now = Timestamp(self.now);
        let r = self.nodes[i].replica.as_mut().expect("up");
        // Adopt the application state only if the replica takes the snapshot: a snapshot
        // that went stale in transit must never rewind the application.
        if r.install_snapshot(now, Slot(snap_through), core_state) == Ok(true) {
            let node = &mut self.nodes[i];
            node.applied = payloads;
            node.next_slot = snap_through + 1;
            // In production: store the snapshot durably before executing the checkpoint
            // `Persist` the replica just queued.
            node.snapshot = snapshot;
        }
        self.drain(i);
    }
}

/// Renders payloads for printing.
pub fn show(payloads: &[Bytes]) -> Vec<String> {
    payloads.iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect()
}

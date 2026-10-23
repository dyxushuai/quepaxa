//! Deterministic simulator and invariant oracles for `quepaxa`.
//!
//! Drives real [`Replica`]s through their public API over a seeded virtual network and
//! virtual disks: messages are delayed, reordered, dropped, duplicated and partitioned;
//! replicas crash (losing unsynced writes, possibly tearing the write in progress) and
//! restart with a fresh seed. After every action the log-level oracles run; after the
//! fault phase the cluster must converge and commit every live proposal.

mod actions;
mod config;
pub mod invariants;
mod kv;
pub mod model;
mod rng;

use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::fmt::Write as _;

use bytes::Bytes;
use quepaxa::{
    Action, Config, Entry, Input, NodeId, Outbound, PersistId, ProposalId, Replica, Slot, Timestamp,
};

pub use config::{Report, SimConfig};
pub use rng::SplitMix64;

const MS: u64 = 1_000_000;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Event {
    Deliver { to: usize, bytes: Vec<u8> },
    PersistDone { node: usize, generation: u64, id: u64 },
    Timer { node: usize, generation: u64, at: u64 },
    Restart { node: usize },
    Kill { node: usize },
    Tick,
}

struct PendingPersist {
    id: u64,
    records: Vec<Bytes>,
    then: Vec<Outbound>,
    replace: bool,
}

/// An application snapshot: the KV state after applying every slot up to `through`.
#[derive(Clone)]
struct Snapshot {
    through: u64,
    state: kv::State,
    core: Bytes,
}

struct Node {
    replica: Option<Replica>,
    generation: u64,
    durable: Vec<Bytes>,
    pending: Vec<PendingPersist>,
    /// When the fsync in progress completes, and when the next (grouped) one will.
    disk_busy_until: u64,
    disk_next_group: Option<u64>,
    timer_at: Option<u64>,
    /// Next slot this node's commit stream must deliver.
    next_commit: u64,
    /// Durable application snapshot (survives crashes).
    snapshot: Option<Snapshot>,
    /// Latest peer snapshot installed (its `through`).
    installed: Option<u64>,
    /// Local proposals of the current incarnation awaiting `Committed`.
    outstanding: HashMap<ProposalId, Bytes>,
    incarnation_payloads: HashMap<ProposalId, Bytes>,
    /// When each local proposal of the current incarnation was made.
    proposed_at: HashMap<ProposalId, u64>,
    /// Membership changes proposed here (they appear in no `Commit`).
    changes: BTreeSet<ProposalId>,
    /// Reported `Retired`: shut down once nothing is being written here.
    retiring: bool,
    /// Shut down after removal (restarted only if a later change adds it back).
    retired: bool,
}

/// A deterministic simulation run.
pub struct Sim {
    cfg: SimConfig,
    rng: SplitMix64,
    now: u64,
    seq: u64,
    queue: BinaryHeap<std::cmp::Reverse<(u64, u64, Event)>>,
    nodes: Vec<Node>,
    cut: BTreeSet<usize>,
    faults_on: bool,
    /// The agreed log, as first committed by any replica.
    ledger: invariants::Ledger,
    report: Report,
    trace: String,
    last_new_slot_at: u64,
    kv: kv::Kv,
    pending_installs: Vec<(usize, usize, u64)>,
    history: invariants::History,
    /// The membership in force (clients, faults and convergence use it) and its epoch.
    members: Vec<usize>,
    epoch: u64,
    /// Every announced epoch: first slot and members. All replicas must agree on them.
    epochs: BTreeMap<u64, (u64, Vec<usize>)>,
}

fn cluster_id() -> [u8; 16] {
    *b"quepaxa-sim-test"
}

impl Sim {
    /// Builds a fresh cluster.
    pub fn new(cfg: SimConfig) -> Self {
        let rng = SplitMix64::new(cfg.seed);
        let nodes_len = cfg.nodes + cfg.spares;
        let initial: Vec<usize> = (0..cfg.nodes).collect();
        let nodes = (0..nodes_len)
            .map(|_| Node {
                replica: None,
                generation: 0,
                durable: Vec::new(),
                pending: Vec::new(),
                disk_busy_until: 0,
                disk_next_group: None,
                timer_at: None,
                next_commit: 0,
                snapshot: None,
                installed: None,
                outstanding: HashMap::new(),
                incarnation_payloads: HashMap::new(),
                proposed_at: HashMap::new(),
                changes: BTreeSet::new(),
                retiring: false,
                retired: false,
            })
            .collect();
        let mut sim = Self {
            cfg,
            rng,
            now: 0,
            seq: 0,
            queue: BinaryHeap::new(),
            nodes,
            cut: BTreeSet::new(),
            faults_on: true,
            ledger: invariants::Ledger::default(),
            report: Report::default(),
            trace: String::new(),
            last_new_slot_at: 0,
            kv: kv::Kv::new(nodes_len),
            pending_installs: Vec::new(),
            history: invariants::History::default(),
            members: initial,
            epoch: 0,
            epochs: BTreeMap::new(),
        };
        if let Some((node, at)) = sim.cfg.kill {
            sim.push(at * MS, Event::Kill { node });
        }
        for i in 0..sim.nodes.len() {
            if sim.cfg.absent != Some(i) {
                sim.start(i);
            }
        }
        sim.push(0, Event::Tick);
        sim
    }

    /// Runs the fault phase, heals, and checks convergence.
    pub fn run(mut self) -> Result<Report, String> {
        let fault_end = self.cfg.fault_phase_ms * MS;
        let heal_end = fault_end + self.cfg.heal_budget_ms * MS;
        let mut same_time = 0u32;
        while let Some(std::cmp::Reverse((at, _, ev))) = self.queue.pop() {
            if at > heal_end {
                break;
            }
            same_time = if at == self.now { same_time + 1 } else { 0 };
            if same_time > 100_000 {
                return Err(self.fail("livelock: events keep firing without time advancing"));
            }
            self.now = at;
            if self.faults_on && self.now >= fault_end {
                self.heal();
            }
            self.dispatch(ev)?;
            if !self.faults_on && self.converged() {
                self.report.slots = self.ledger.slots() as u64;
                self.report.entries = self.ledger.entries();
                self.report.proposals = self.ledger.proposals();
                self.report.trace_hash = fnv(self.trace.as_bytes());
                let lines: Vec<&str> = self.trace.lines().collect();
                self.report.trace_tail = lines[lines.len().saturating_sub(400)..].join("\n");
                self.report.reads = self.kv.reads();
                self.report.local_batches = self
                    .nodes
                    .iter()
                    .filter_map(|n| n.replica.as_ref().map(|r| r.inspect().local_batches))
                    .max()
                    .unwrap_or(0);
                self.report.metrics = self
                    .nodes
                    .iter()
                    .filter_map(|n| n.replica.as_ref().map(|r| r.metrics().clone()))
                    .collect();
                if let Err(e) = self.kv.check() {
                    return Err(self.fail(&format!("linearizability: {e}")));
                }
                return Ok(self.report);
            }
        }
        Err(self.fail("did not converge after faults stopped"))
    }

    fn fail(&self, what: &str) -> String {
        let tail: Vec<&str> = self
            .trace
            .lines()
            .rev()
            .take(std::env::var("QPX_TAIL").ok().and_then(|s| s.parse().ok()).unwrap_or(40))
            .collect();
        let nodes: Vec<String> = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let ci = n.replica.as_ref().map(|r| r.commit_index());
                format!(
                    "n{i}: commit_index={ci:?} next_commit={} outstanding={} pending_persists={}",
                    n.next_commit,
                    n.outstanding.len(),
                    n.pending.len()
                )
            })
            .collect();
        format!(
            "seed {}: {what} (agreed log {} slots)\n{}\n…\n{}",
            self.cfg.seed,
            self.ledger.slots(),
            nodes.join("\n"),
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        )
    }

    fn push(&mut self, at: u64, ev: Event) {
        self.seq += 1;
        self.queue.push(std::cmp::Reverse((at, self.seq, ev)));
    }

    fn chance(&mut self, per_mille: u32) -> bool {
        self.rng.below(1000) < u64::from(per_mille)
    }

    fn heal(&mut self) {
        self.faults_on = false;
        self.cut.clear();
        let _ = writeln!(self.trace, "{} heal", self.now);
        for i in 0..self.nodes.len() {
            if self.nodes[i].replica.is_none() {
                self.push(self.now, Event::Restart { node: i });
            }
        }
    }

    fn converged(&self) -> bool {
        let target = self.ledger.slots() as u64;
        self.queue.iter().all(|std::cmp::Reverse((_, _, e))| !matches!(e, Event::Restart { .. }))
            && self.members.iter().map(|&i| &self.nodes[i]).all(|n| {
                n.replica.as_ref().is_some_and(|r| r.commit_index() == target)
                    && n.next_commit == target
                    && n.outstanding.is_empty()
            })
            && self.kv.no_pending_reads()
    }

    fn config(&mut self, i: usize) -> Config {
        let members = (0..self.cfg.nodes as u32).map(NodeId).collect();
        let mut seed = [0u8; 32];
        for chunk in seed.chunks_mut(8) {
            chunk.copy_from_slice(&self.rng.next_u64().to_le_bytes());
        }
        let mut cfg = Config::new(cluster_id(), NodeId(i as u32), members, seed);
        cfg.retransmit_interval = self.cfg.retransmit_ms * MS;
        cfg.hedge_max = self.cfg.hedge_max_ms * MS;
        cfg.max_in_flight_slots = self.cfg.in_flight_slots;
        cfg.max_batch_bytes = 4096;
        cfg.max_message_bytes = self.cfg.max_message_bytes;
        cfg
    }

    fn start(&mut self, i: usize) {
        let cfg = self.config(i);
        let records = self.nodes[i].durable.clone();
        let replica = Replica::new(cfg, records, Timestamp(self.now)).expect("durable records must recover");
        let node = &mut self.nodes[i];
        node.replica = Some(replica);
        node.generation += 1;
        node.outstanding.clear();
        self.kv.restarted(i);
        node.next_commit = match &node.snapshot {
            Some(snap) => {
                self.kv.set_state(i, snap.state.clone());
                snap.through + 1
            }
            None => 0,
        };
        node.incarnation_payloads.clear();
        node.proposed_at.clear();
        node.changes.clear();
        node.timer_at = None;
        let _ = writeln!(self.trace, "{} start n{i} g{}", self.now, node.generation);
        self.drain(i);
        // The application's snapshot may be newer than the replica's last durable compaction.
        if let Some(snap) = self.nodes[i].snapshot.clone()
            && let Some(r) = self.nodes[i].replica.as_mut()
        {
            r.install_snapshot(Timestamp(self.now), Slot(snap.through), snap.core)
                .expect("own snapshot is valid");
            self.drain(i);
        }
    }

    fn crash(&mut self, i: usize) {
        let node = &mut self.nodes[i];
        if node.replica.take().is_none() {
            return;
        }
        self.history.forget(self.cfg.nodes, i);
        // The write in progress may be torn: a prefix of its records reaches the disk.
        let pending = std::mem::take(&mut node.pending);
        if let Some(first) = pending.into_iter().next() {
            if first.replace {
                // Replacement is atomic (write-new-then-rename): all or nothing.
                if self.rng.below(2) == 0 {
                    self.nodes[i].durable = first.records;
                }
            } else {
                let keep = self.rng.below(first.records.len() as u64 + 1) as usize;
                self.nodes[i].durable.extend(first.records.into_iter().take(keep));
            }
        }
        let node = &mut self.nodes[i];
        node.generation += 1;
        node.disk_busy_until = self.now;
        node.disk_next_group = None;
        self.report.crashes += 1;
        let _ = writeln!(self.trace, "{} crash n{i}", self.now);
        let down = 50 * MS + self.rng.below(500 * MS);
        self.push(self.now + down, Event::Restart { node: i });
    }

    fn dispatch(&mut self, ev: Event) -> Result<(), String> {
        match ev {
            Event::Tick => {
                if self.faults_on {
                    self.inject_faults();
                }
                if self.faults_on && self.chance(self.cfg.propose) {
                    let i = self.member();
                    self.propose(i)?;
                }
                if self.faults_on && self.chance(self.cfg.read) {
                    let i = self.member();
                    self.read(i)?;
                }
                // Checked only when enabled, so other configurations keep their random streams.
                if self.faults_on && self.cfg.reconfigure > 0 && self.chance(self.cfg.reconfigure) {
                    self.reconfigure()?;
                }
                if self.faults_on {
                    self.push(self.now + MS, Event::Tick);
                }
            }
            Event::Restart { node } => {
                let killed =
                    self.faults_on && self.cfg.kill.is_some_and(|(k, at)| k == node && self.now >= at * MS);
                if self.nodes[node].replica.is_none() && !killed && !self.nodes[node].retired {
                    self.start(node);
                }
            }
            Event::Kill { node } => self.crash(node),
            Event::Deliver { to, bytes } => {
                let Some(r) = self.nodes[to].replica.as_mut() else {
                    return Ok(());
                };
                r.handle(Timestamp(self.now), Input::Message(Bytes::from(bytes)))
                    .map_err(|e| e.to_string())?;
                self.drain(to);
            }
            Event::PersistDone { node, generation, id } => {
                if self.nodes[node].generation != generation {
                    return Ok(());
                }
                let n = &mut self.nodes[node];
                let pos = n.pending.iter().position(|p| p.id == id).expect("persist completes once");
                let p = n.pending.remove(pos);
                if p.replace {
                    n.durable = p.records;
                } else {
                    n.durable.extend(p.records);
                }
                for out in p.then {
                    self.net_send(node, out);
                }
                let r = self.nodes[node].replica.as_mut().expect("live generation");
                r.handle(Timestamp(self.now), Input::Persisted(PersistId(id))).map_err(|e| e.to_string())?;
                self.drain(node);
            }
            Event::Timer { node, generation, at } => {
                let n = &mut self.nodes[node];
                if n.generation != generation || n.timer_at != Some(at) {
                    return Ok(());
                }
                n.timer_at = None;
                let r = n.replica.as_mut().expect("live generation");
                r.handle_timeout(Timestamp(self.now)).map_err(|e| e.to_string())?;
                self.drain(node);
            }
        }
        self.check_internal()
    }

    /// G1, G3–G5 across every live replica's protocol state.
    fn check_internal(&mut self) -> Result<(), String> {
        let live: Vec<_> = self
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| n.replica.as_ref().map(|r| (i, r.inspect())))
            .collect();
        let result = self.history.observe(self.cfg.nodes, &live);
        result.map_err(|e| self.fail(&e))
    }

    /// A random member of the latest membership.
    fn member(&mut self) -> usize {
        self.members[self.rng.below(self.members.len() as u64) as usize]
    }

    /// Proposes `nodes` replicas drawn from everyone, at a random member.
    fn reconfigure(&mut self) -> Result<(), String> {
        let mut pool: Vec<usize> = (0..self.nodes.len()).collect();
        for k in (1..pool.len()).rev() {
            pool.swap(k, self.rng.below(k as u64 + 1) as usize);
        }
        pool.truncate(self.cfg.nodes);
        let i = self.member();
        let Some(r) = self.nodes[i].replica.as_mut() else { return Ok(()) };
        let members = pool.iter().map(|&m| NodeId(m as u32)).collect();
        match r.propose_reconfiguration(Timestamp(self.now), members, Vec::new()) {
            Ok(id) => {
                self.nodes[i].changes.insert(id);
                let _ = writeln!(self.trace, "{} n{i} proposes members {pool:?}", self.now);
            }
            Err(quepaxa::Error::ReconfigurationPending | quepaxa::Error::Busy) => {}
            Err(e) => return Err(self.fail(&format!("propose_reconfiguration failed: {e}"))),
        }
        self.drain(i);
        Ok(())
    }

    fn inject_faults(&mut self) {
        let n = self.cfg.nodes;
        if self.cut.is_empty() && n > 2 && self.chance(self.cfg.partition) {
            while self.cut.len() < (n - 1) / 2 {
                let x = self.member();
                self.cut.insert(x);
            }
            let _ = writeln!(self.trace, "{} partition {:?}", self.now, self.cut);
        } else if !self.cut.is_empty() && self.chance(20) {
            self.cut.clear();
            let _ = writeln!(self.trace, "{} heal-partition", self.now);
        }
        // Keep a majority up so the fault phase still makes progress.
        let down = self.members.iter().filter(|&&m| self.nodes[m].replica.is_none()).count();
        if down < (n - 1) / 2 && self.chance(self.cfg.crash) {
            let i = self.member();
            self.crash(i);
        }
    }

    fn propose(&mut self, i: usize) -> Result<(), String> {
        let n = self.ledger.proposals();
        let payload = kv::write_payload(n as u64 % kv::KEYS, &format!("n{i}-p{n}"));
        let Some(r) = self.nodes[i].replica.as_mut() else {
            return Ok(());
        };
        match r.propose(Timestamp(self.now), payload.clone()) {
            Ok(id) => {
                self.ledger.proposed(payload.clone());
                self.kv.write_invoked(payload.clone());
                self.nodes[i].outstanding.insert(id, payload.clone());
                self.nodes[i].incarnation_payloads.insert(id, payload);
                self.nodes[i].proposed_at.insert(id, self.now);
            }
            Err(quepaxa::Error::Busy) => {}
            Err(e) => return Err(self.fail(&format!("propose failed: {e}"))),
        }
        self.drain(i);
        Ok(())
    }

    fn read(&mut self, i: usize) -> Result<(), String> {
        let key = b'0' + self.rng.below(kv::KEYS) as u8;
        let Some(r) = self.nodes[i].replica.as_mut() else { return Ok(()) };
        match r.read_barrier(Timestamp(self.now)) {
            Ok(id) => self.kv.read_invoked(i, id, key),
            Err(quepaxa::Error::Busy) => {}
            Err(e) => return Err(self.fail(&format!("read_barrier failed: {e}"))),
        }
        self.drain(i);
        Ok(())
    }

    /// The application snapshots its state (durably, instantly in the sim) and compacts.
    fn maybe_compact(&mut self, i: usize) {
        let Some(every) = self.cfg.compact_every else { return };
        let last = self.nodes[i].snapshot.as_ref().map_or(0, |s| s.through + 1);
        let next = self.nodes[i].next_commit;
        if next < last + every {
            return;
        }
        let Some(core) = self.nodes[i].replica.as_ref().map(Replica::core_state) else { return };
        let through = next - 1;
        let state = self.kv.state(i);
        self.nodes[i].snapshot = Some(Snapshot { through, state, core: core.clone() });
        let r = self.nodes[i].replica.as_mut().expect("checked above");
        r.compact(Timestamp(self.now), Slot(through), core).expect("valid compaction");
        self.drain(i);
    }

    /// Ships snapshots requested by `SnapshotNeeded` (out of band, instantly).
    fn install_snapshots(&mut self) {
        while let Some((i, peer, through)) = self.pending_installs.pop() {
            let Some(snap) = self.nodes[peer].snapshot.clone() else { continue };
            if snap.through < through {
                continue;
            }
            let Some(r) = self.nodes[i].replica.as_mut() else { continue };
            // The application adopts the snapshot only if the replica does (never rewinds).
            if !r
                .install_snapshot(Timestamp(self.now), Slot(snap.through), snap.core.clone())
                .expect("valid snapshot")
            {
                continue;
            }
            self.kv.set_state(i, snap.state.clone());
            self.nodes[i].next_commit = snap.through + 1;
            self.nodes[i].snapshot = Some(snap.clone());
            self.nodes[i].installed = Some(snap.through);
            self.report.snapshots_installed += 1;
            let _ = writeln!(self.trace, "{} n{i} install snapshot through {}", self.now, snap.through);
            self.drain(i);
        }
    }
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

// Compiles and runs the README's code blocks as doctests. It lives here because this crate
// is never published: in the packaged `quepaxa` the README's path would not resolve.
#[cfg(doctest)]
#[doc = include_str!("../../../README.md")]
struct ReadmeDoctests;

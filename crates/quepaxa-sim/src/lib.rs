//! Deterministic simulator and invariant oracles for `quepaxa`.
//!
//! Drives real [`Replica`]s through their public API over a seeded virtual network and
//! virtual disks: messages are delayed, reordered, dropped, duplicated and partitioned;
//! replicas crash (losing unsynced writes, possibly tearing the write in progress) and
//! restart with a fresh seed. After every action the log-level oracles run; after the
//! fault phase the cluster must converge and commit every live proposal.

mod rng;

use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::fmt::Write as _;

use bytes::Bytes;
use quepaxa::{
    Action, Config, Entry, Input, NodeId, Outbound, PersistId, ProposalId, Replica, Slot, Timestamp,
};

pub use rng::SplitMix64;

const MS: u64 = 1_000_000;

/// Fault and workload knobs. Rates are per mille.
#[derive(Clone, Debug)]
pub struct SimConfig {
    /// Cluster size.
    pub nodes: usize,
    /// Master seed; the same seed replays the same run.
    pub seed: u64,
    /// Virtual duration of the fault phase.
    pub fault_phase_ms: u64,
    /// Virtual time allowed to converge after faults stop.
    pub heal_budget_ms: u64,
    /// Probability that a sent message is lost.
    pub drop: u32,
    /// Probability that a sent message is duplicated.
    pub dup: u32,
    /// Probability per tick that a minority is partitioned off.
    pub partition: u32,
    /// Probability per tick that some replica crashes.
    pub crash: u32,
    /// Probability per tick that some replica receives a client proposal.
    pub propose: u32,
    /// Upper bound on one-way message latency.
    pub max_latency_ms: u64,
    /// Upper bound on one persist's latency.
    pub max_persist_ms: u64,
    /// Replica limit on concurrent slots.
    pub in_flight_slots: u32,
}

impl SimConfig {
    /// Long fsyncs, frequent crashes and heavy loss: maximizes the window in which a reply
    /// could escape before its write is durable.
    pub fn durability_stress(nodes: usize, seed: u64) -> Self {
        Self {
            drop: 250,
            crash: 25,
            max_persist_ms: 40,
            in_flight_slots: 2,
            ..Self::hostile(nodes, seed)
        }
    }

    /// A hostile default: lossy, reordering, partitioning, crashing.
    pub fn hostile(nodes: usize, seed: u64) -> Self {
        Self {
            nodes,
            seed,
            fault_phase_ms: 3_000,
            heal_budget_ms: 60_000,
            drop: 50,
            dup: 20,
            partition: 3,
            crash: 3,
            propose: 300,
            max_latency_ms: 20,
            max_persist_ms: 5,
            in_flight_slots: 4,
        }
    }
}

/// What a finished run observed.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Slots in the agreed log.
    pub slots: u64,
    /// Entries committed.
    pub entries: usize,
    /// Proposals made.
    pub proposals: usize,
    /// Crashes injected.
    pub crashes: usize,
    /// Fingerprint of the event trace (same seed ⇒ same fingerprint).
    pub trace_hash: u64,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Event {
    Deliver {
        to: usize,
        bytes: Vec<u8>,
    },
    PersistDone {
        node: usize,
        generation: u64,
        id: u64,
    },
    Timer {
        node: usize,
        generation: u64,
        at: u64,
    },
    Restart {
        node: usize,
    },
    Tick,
}

struct PendingPersist {
    id: u64,
    records: Vec<Bytes>,
    then: Vec<Outbound>,
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
    /// Local proposals of the current incarnation awaiting `Committed`.
    outstanding: HashMap<ProposalId, Bytes>,
    incarnation_payloads: HashMap<ProposalId, Bytes>,
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
    log: BTreeMap<u64, Vec<Entry>>,
    proposed: BTreeSet<Bytes>,
    committed_payloads: BTreeSet<Bytes>,
    report: Report,
    trace: String,
}

fn cluster_id() -> [u8; 16] {
    *b"quepaxa-sim-test"
}

impl Sim {
    /// Builds a fresh cluster.
    pub fn new(cfg: SimConfig) -> Self {
        let rng = SplitMix64::new(cfg.seed);
        let nodes = (0..cfg.nodes)
            .map(|_| Node {
                replica: None,
                generation: 0,
                durable: Vec::new(),
                pending: Vec::new(),
                disk_busy_until: 0,
                disk_next_group: None,
                timer_at: None,
                next_commit: 0,
                outstanding: HashMap::new(),
                incarnation_payloads: HashMap::new(),
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
            log: BTreeMap::new(),
            proposed: BTreeSet::new(),
            committed_payloads: BTreeSet::new(),
            report: Report::default(),
            trace: String::new(),
        };
        for i in 0..sim.cfg.nodes {
            sim.start(i);
        }
        sim.push(0, Event::Tick);
        sim
    }

    /// Runs the fault phase, heals, and checks convergence.
    pub fn run(mut self) -> Result<Report, String> {
        let fault_end = self.cfg.fault_phase_ms * MS;
        let heal_end = fault_end + self.cfg.heal_budget_ms * MS;
        while let Some(std::cmp::Reverse((at, _, ev))) = self.queue.pop() {
            if at > heal_end {
                break;
            }
            self.now = at;
            if self.faults_on && self.now >= fault_end {
                self.heal();
            }
            self.dispatch(ev)?;
            if !self.faults_on && self.converged() {
                self.report.slots = self.log.len() as u64;
                self.report.entries = self.committed_payloads.len();
                self.report.proposals = self.proposed.len();
                self.report.trace_hash = fnv(self.trace.as_bytes());
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
            .take(
                std::env::var("QPX_TAIL")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(40),
            )
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
            self.log.len(),
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
        let target = self.log.len() as u64;
        self.queue
            .iter()
            .all(|std::cmp::Reverse((_, _, e))| !matches!(e, Event::Restart { .. }))
            && self.nodes.iter().all(|n| {
                n.replica
                    .as_ref()
                    .is_some_and(|r| r.commit_index() == target)
                    && n.next_commit == target
                    && n.outstanding.is_empty()
            })
    }

    fn config(&mut self, i: usize) -> Config {
        let members = (0..self.cfg.nodes as u32).map(NodeId).collect();
        let mut seed = [0u8; 32];
        for chunk in seed.chunks_mut(8) {
            chunk.copy_from_slice(&self.rng.next_u64().to_le_bytes());
        }
        let mut cfg = Config::new(cluster_id(), NodeId(i as u32), members, seed);
        cfg.retransmit_interval = 20 * MS;
        cfg.stall_timeout = 60 * MS;
        cfg.max_in_flight_slots = self.cfg.in_flight_slots;
        cfg.max_batch_bytes = 4096;
        cfg.max_message_bytes = 1 << 20;
        cfg
    }

    fn start(&mut self, i: usize) {
        let cfg = self.config(i);
        let records = self.nodes[i].durable.clone();
        let replica =
            Replica::new(cfg, records, Timestamp(self.now)).expect("durable records must recover");
        let node = &mut self.nodes[i];
        node.replica = Some(replica);
        node.generation += 1;
        node.next_commit = 0;
        node.outstanding.clear();
        node.incarnation_payloads.clear();
        node.timer_at = None;
        let _ = writeln!(self.trace, "{} start n{i} g{}", self.now, node.generation);
        self.drain(i);
    }

    fn crash(&mut self, i: usize) {
        let node = &mut self.nodes[i];
        if node.replica.take().is_none() {
            return;
        }
        // The write in progress may be torn: a prefix of its records reaches the disk.
        let pending = std::mem::take(&mut node.pending);
        if let Some(first) = pending.into_iter().next() {
            let keep = self.rng.below(first.records.len() as u64 + 1) as usize;
            self.nodes[i]
                .durable
                .extend(first.records.into_iter().take(keep));
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
                    let i = self.rng.below(self.cfg.nodes as u64) as usize;
                    self.propose(i)?;
                }
                if self.faults_on {
                    self.push(self.now + MS, Event::Tick);
                }
            }
            Event::Restart { node } => {
                if self.nodes[node].replica.is_none() {
                    self.start(node);
                }
            }
            Event::Deliver { to, bytes } => {
                let Some(r) = self.nodes[to].replica.as_mut() else {
                    return Ok(());
                };
                r.handle(Timestamp(self.now), Input::Message(Bytes::from(bytes)))
                    .map_err(|e| e.to_string())?;
                self.drain(to);
            }
            Event::PersistDone {
                node,
                generation,
                id,
            } => {
                if self.nodes[node].generation != generation {
                    return Ok(());
                }
                let n = &mut self.nodes[node];
                let pos = n
                    .pending
                    .iter()
                    .position(|p| p.id == id)
                    .expect("persist completes once");
                let p = n.pending.remove(pos);
                n.durable.extend(p.records);
                for out in p.then {
                    self.net_send(node, out);
                }
                let r = self.nodes[node].replica.as_mut().expect("live generation");
                r.handle(Timestamp(self.now), Input::Persisted(PersistId(id)))
                    .map_err(|e| e.to_string())?;
                self.drain(node);
            }
            Event::Timer {
                node,
                generation,
                at,
            } => {
                let n = &mut self.nodes[node];
                if n.generation != generation || n.timer_at != Some(at) {
                    return Ok(());
                }
                n.timer_at = None;
                let r = n.replica.as_mut().expect("live generation");
                r.handle_timeout(Timestamp(self.now))
                    .map_err(|e| e.to_string())?;
                self.drain(node);
            }
        }
        self.check_global()
    }

    fn inject_faults(&mut self) {
        let n = self.cfg.nodes;
        if self.cut.is_empty() && n > 2 && self.chance(self.cfg.partition) {
            while self.cut.len() < (n - 1) / 2 {
                let x = self.rng.below(n as u64) as usize;
                self.cut.insert(x);
            }
            let _ = writeln!(self.trace, "{} partition {:?}", self.now, self.cut);
        } else if !self.cut.is_empty() && self.chance(20) {
            self.cut.clear();
            let _ = writeln!(self.trace, "{} heal-partition", self.now);
        }
        // Keep a majority up so the fault phase still makes progress.
        let down = self.nodes.iter().filter(|x| x.replica.is_none()).count();
        if down < (n - 1) / 2 && self.chance(self.cfg.crash) {
            let i = self.rng.below(n as u64) as usize;
            self.crash(i);
        }
    }

    fn propose(&mut self, i: usize) -> Result<(), String> {
        let payload = Bytes::from(format!("n{i}-p{}", self.proposed.len()));
        let Some(r) = self.nodes[i].replica.as_mut() else {
            return Ok(());
        };
        match r.propose(Timestamp(self.now), payload.clone()) {
            Ok(id) => {
                self.proposed.insert(payload.clone());
                self.nodes[i].outstanding.insert(id, payload.clone());
                self.nodes[i].incarnation_payloads.insert(id, payload);
            }
            Err(quepaxa::Error::Busy) => {}
            Err(e) => return Err(self.fail(&format!("propose failed: {e}"))),
        }
        self.drain(i);
        Ok(())
    }

    fn net_send(&mut self, from: usize, out: Outbound) {
        let to = out.to.0 as usize;
        if self.faults_on
            && (self.cut.contains(&from) != self.cut.contains(&to) || self.chance(self.cfg.drop))
        {
            return;
        }
        let copies = if self.faults_on && self.chance(self.cfg.dup) {
            2
        } else {
            1
        };
        for _ in 0..copies {
            let at = self.now + 1 + self.rng.below(self.cfg.max_latency_ms * MS);
            self.push(
                at,
                Event::Deliver {
                    to,
                    bytes: out.bytes.to_vec(),
                },
            );
        }
    }

    fn drain(&mut self, i: usize) {
        let mut actions = Vec::new();
        let Some(r) = self.nodes[i].replica.as_mut() else {
            return;
        };
        r.poll_actions(&mut actions);
        let deadline = r.next_deadline();
        for a in actions {
            self.on_action(i, a);
        }
        let n = &mut self.nodes[i];
        if let Some(Timestamp(at)) = deadline {
            let at = at.max(self.now);
            if n.timer_at != Some(at) {
                n.timer_at = Some(at);
                let generation = n.generation;
                self.push(
                    at,
                    Event::Timer {
                        node: i,
                        generation,
                        at,
                    },
                );
            }
        }
    }

    fn on_action(&mut self, i: usize, a: Action) {
        match a {
            Action::Send(out) => self.net_send(i, out),
            Action::Persist { id, records, then } => {
                // Group commit: writes issued while an fsync runs share the next one.
                let lat = 1 + self.rng.below(self.cfg.max_persist_ms * MS);
                let n = &mut self.nodes[i];
                if n.disk_next_group.is_some_and(|g| g <= self.now) || self.now >= n.disk_busy_until
                {
                    n.disk_busy_until = n.disk_busy_until.max(n.disk_next_group.unwrap_or(0));
                    n.disk_next_group = None;
                }
                let done = if self.now >= n.disk_busy_until {
                    n.disk_busy_until = self.now + lat;
                    n.disk_busy_until
                } else {
                    *n.disk_next_group.get_or_insert(n.disk_busy_until + lat)
                };
                n.pending.push(PendingPersist {
                    id: id.0,
                    records,
                    then,
                });
                let generation = n.generation;
                self.push(
                    done,
                    Event::PersistDone {
                        node: i,
                        generation,
                        id: id.0,
                    },
                );
            }
            Action::Commit { slot, entries } => self.on_commit(i, slot, entries),
            Action::Committed { id, slot } => {
                let n = &mut self.nodes[i];
                let payload = n.incarnation_payloads.get(&id).cloned();
                n.outstanding.remove(&id);
                let in_log = payload.is_some_and(|p| {
                    self.log
                        .get(&slot.0)
                        .is_some_and(|es| es.iter().any(|e| e.payload == p))
                });
                assert!(
                    in_log,
                    "{}",
                    self.fail(&format!(
                        "n{i} Committed {id:?} in {slot:?} but not in the log"
                    ))
                );
            }
            Action::Dropped { id, .. } => {
                self.nodes[i].outstanding.remove(&id);
            }
            _ => {}
        }
    }

    fn on_commit(&mut self, i: usize, slot: Slot, entries: Vec<Entry>) {
        let expected = self.nodes[i].next_commit;
        assert_eq!(
            slot.0,
            expected,
            "{}",
            self.fail(&format!("n{i} commit out of order"))
        );
        let n = &mut self.nodes[i];
        n.next_commit += 1;
        let _ = writeln!(
            self.trace,
            "{} n{i} commit {} ({} entries)",
            self.now,
            slot.0,
            entries.len()
        );
        match self.log.get(&slot.0) {
            Some(agreed) => assert_eq!(
                agreed,
                &entries,
                "{}",
                self.fail(&format!("agreement broken at slot {}", slot.0))
            ),
            None => {
                for e in &entries {
                    assert!(
                        self.proposed.contains(&e.payload),
                        "{}",
                        self.fail("validity: unknown payload")
                    );
                    assert!(
                        self.committed_payloads.insert(e.payload.clone()),
                        "{}",
                        self.fail(&format!("payload {:?} committed twice", e.payload))
                    );
                }
                self.log.insert(slot.0, entries);
            }
        }
    }

    fn check_global(&self) -> Result<(), String> {
        // The agreed log has no holes below its highest slot once every replica passed it.
        Ok(())
    }
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

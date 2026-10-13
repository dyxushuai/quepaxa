//! Bounded systematic exploration of real replicas (delay-bounded model checking).
//!
//! A deterministic default scheduler runs events in arrival order (message deliveries and
//! persist completions) and fires the earliest timer when nothing else is pending. The
//! explorer enumerates every schedule that deviates from it at most `deviations` times,
//! where a deviation is: delivering or completing any other pending event first, losing a
//! message, duplicating one, firing a timer early, or crashing a replica (losing any
//! suffix of the write in progress) and restarting it. Most concurrency bugs need only a
//! few deviations, which makes this far more effective than unguided interleaving search.
//! States are deduplicated by fingerprint; every state is checked for agreement, ordered
//! commits, and `Committed` only for logged entries.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::hash::{Hash, Hasher};

use bytes::Bytes;
use quepaxa::{Action, Config, Entry, Input, NodeId, PersistId, ProposalId, Replica, Timestamp};

/// Bounds of one exploration.
#[derive(Clone, Debug)]
pub struct ModelConfig {
    /// Cluster size.
    pub nodes: usize,
    /// One client write per entry, proposed at that replica at the start.
    pub proposals: Vec<usize>,
    /// Deviations from the default schedule along one path.
    pub deviations: u32,
    /// Crash-restarts allowed along one path (a separate budget; they cost no deviation).
    pub crashes: u32,
    /// Longest path explored.
    pub max_depth: usize,
    /// Distinct states explored before stopping.
    pub max_states: usize,
}

/// What an exploration covered.
#[derive(Clone, Debug, Default)]
pub struct ModelReport {
    /// Distinct states visited.
    pub states: usize,
    /// States in which every replica committed every proposal.
    pub complete: usize,
    /// Whether every schedule within the bounds was explored.
    pub exhausted: bool,
    /// States at the depth bound; each was shown to settle under the default schedule.
    pub depth_limited: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Event {
    Deliver { from: usize, to: usize, bytes: Bytes },
    Persisted { node: usize, id: PersistId, records: Vec<Bytes>, then: Vec<(usize, Bytes)>, replace: bool },
}

#[derive(Clone)]
struct MNode {
    replica: Replica,
    durable: Vec<Bytes>,
    now: u64,
    next_commit: u64,
    starts: u64,
    /// Own proposals of the current incarnation, by id.
    proposed: BTreeMap<ProposalId, Bytes>,
    /// Proposals whose client received `Committed`.
    acked: BTreeSet<ProposalId>,
}

#[derive(Clone)]
struct State {
    nodes: Vec<MNode>,
    events: VecDeque<Event>,
    deviations: u32,
    crashes: u32,
    isolated: Option<usize>,
    history: crate::invariants::History,
    log: BTreeMap<u64, Vec<Entry>>,
    /// Slot each committed payload first appeared in.
    committed_at: BTreeMap<Bytes, u64>,
    /// Every payload any client proposed (validity).
    proposals: Vec<Bytes>,
    /// Payloads that must eventually commit: client writes, minus those whose replica
    /// crashed before committing them (the client retries those as new payloads).
    required: Vec<Bytes>,
    depth: usize,
}

#[derive(Clone, Copy, Debug)]
enum Step {
    Run(usize),
    Lose(usize),
    Duplicate(usize),
    Timeout(usize),
    /// Cuts a replica off the network from now on (one deviation).
    Isolate(usize),
    /// Ends the partition (the default step once nothing else is pending).
    Heal,
    /// Crash-restart, losing a suffix of the write in progress (its own budget).
    Crash {
        node: usize,
        keep: usize,
    },
}

fn config(n: usize, i: usize, starts: u64) -> Config {
    let mut seed = [0u8; 32];
    seed[0] = i as u8;
    seed[1..9].copy_from_slice(&starts.to_le_bytes());
    let members = (0..n as u32).map(NodeId).collect();
    let mut cfg = Config::new(*b"quepaxa-modelchk", NodeId(i as u32), members, seed);
    cfg.max_in_flight_slots = 2;
    cfg
}

impl State {
    fn fingerprint(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for n in &self.nodes {
            (n.replica.fingerprint(), &n.durable, n.now, n.next_commit, &n.proposed).hash(&mut h);
        }
        (&self.events, self.deviations, self.crashes, self.isolated, &self.log, &self.history).hash(&mut h);
        h.finish()
    }

    /// Crash-restarts are bounded by their own budget, not by deviations.
    fn crash_steps(&self, steps: &mut Vec<(Step, bool)>) {
        if self.crashes == 0 {
            return;
        }
        for i in 0..self.nodes.len() {
            let first = self.events.iter().find_map(|e| match e {
                Event::Persisted { node, records, replace, .. } if *node == i => {
                    Some((records.len(), *replace))
                }
                _ => None,
            });
            let keeps = match first {
                None => 1,
                Some((_, true)) => 2,
                Some((n, false)) => n + 1,
            };
            for keep in 0..keeps {
                steps.push((Step::Crash { node: i, keep }, false));
            }
        }
    }

    /// Queues a message unless either end is cut off.
    fn post(&mut self, from: usize, to: usize, bytes: Bytes) {
        if self.isolated != Some(from) && self.isolated != Some(to) {
            self.events.push_back(Event::Deliver { from, to, bytes });
        }
    }

    /// The default step, then every deviation still affordable.
    fn steps(&self) -> Vec<(Step, bool)> {
        let earliest_timer = || {
            (0..self.nodes.len())
                .filter_map(|i| self.nodes[i].replica.next_deadline().map(|t| (t, i)))
                .min()
                .map(|(_, i)| i)
        };
        let mut steps = Vec::new();
        match (self.events.is_empty(), earliest_timer()) {
            // The front event is always runnable: no earlier persist of its node precedes it.
            (false, _) => steps.push((Step::Run(0), false)),
            (true, Some(i)) => steps.push((Step::Timeout(i), false)),
            // A partition that outlives all other activity heals.
            (true, None) if self.isolated.is_some() => steps.push((Step::Heal, false)),
            (true, None) => {}
        }
        self.crash_steps(&mut steps);
        if self.deviations == 0 {
            return steps;
        }
        let mut persist_seen = HashSet::new();
        for (k, e) in self.events.iter().enumerate() {
            // Persists of one replica complete in order (the caller contract).
            let in_order = match e {
                Event::Persisted { node, .. } => persist_seen.insert(*node),
                Event::Deliver { .. } => true,
            };
            if k > 0 && in_order {
                steps.push((Step::Run(k), true));
            }
            if matches!(e, Event::Deliver { .. }) {
                steps.push((Step::Lose(k), true));
                steps.push((Step::Duplicate(k), true));
            }
        }
        let default_timer = if self.events.is_empty() { earliest_timer() } else { None };
        for i in 0..self.nodes.len() {
            if Some(i) != default_timer && self.nodes[i].replica.next_deadline().is_some() {
                steps.push((Step::Timeout(i), true));
            }
            if self.isolated.is_none() {
                steps.push((Step::Isolate(i), true));
            }
        }
        steps
    }

    fn apply(&mut self, step: Step) -> Result<(), String> {
        self.depth += 1;
        match step {
            Step::Run(k) => match self.events.remove(k).expect("offered event exists") {
                Event::Deliver { to, bytes, .. } => {
                    let n = &mut self.nodes[to];
                    n.replica.handle(Timestamp(n.now), Input::Message(bytes)).map_err(|e| e.to_string())?;
                    self.drain(to)
                }
                Event::Persisted { node, id, records, then, replace } => {
                    let n = &mut self.nodes[node];
                    if replace {
                        n.durable = records;
                    } else {
                        n.durable.extend(records);
                    }
                    for (to, bytes) in then {
                        self.post(node, to, bytes);
                    }
                    let n = &mut self.nodes[node];
                    n.replica.handle(Timestamp(n.now), Input::Persisted(id)).map_err(|e| e.to_string())?;
                    self.drain(node)
                }
            },
            Step::Lose(k) => {
                self.events.remove(k);
                Ok(())
            }
            Step::Duplicate(k) => {
                let e = self.events[k].clone();
                self.events.push_back(e);
                Ok(())
            }
            Step::Timeout(i) => {
                let n = &mut self.nodes[i];
                let Some(Timestamp(at)) = n.replica.next_deadline() else { return Ok(()) };
                n.now = n.now.max(at);
                n.replica.handle_timeout(Timestamp(n.now)).map_err(|e| e.to_string())?;
                self.drain(i)
            }
            Step::Heal => {
                self.isolated = None;
                Ok(())
            }
            Step::Isolate(node) => {
                self.isolated = Some(node);
                self.events.retain(
                    |e| !matches!(e, Event::Deliver { from, to, .. } if *from == node || *to == node),
                );
                Ok(())
            }
            Step::Crash { node, keep } => self.crash(node, keep),
        }
    }

    fn crash(&mut self, node: usize, keep: usize) -> Result<(), String> {
        self.crashes -= 1;
        self.history.forget(self.nodes.len(), node);
        let mut first = true;
        let mut kept = Vec::new();
        let mut replaced = None;
        self.events.retain(|e| match e {
            Event::Persisted { node: n, records, replace, .. } if *n == node => {
                if first {
                    first = false;
                    if *replace {
                        replaced = (keep == 1).then(|| records.clone());
                    } else {
                        kept = records[..keep].to_vec();
                    }
                }
                false
            }
            _ => true,
        });
        let total = self.nodes.len();
        let n = &mut self.nodes[node];
        if let Some(records) = replaced {
            n.durable = records;
        }
        n.durable.extend(kept);
        n.starts += 1;
        n.next_commit = 0;
        // The client retries exactly the writes it never saw acknowledged.
        let acked = std::mem::take(&mut n.acked);
        let lost: Vec<Bytes> = std::mem::take(&mut n.proposed)
            .into_iter()
            .filter(|(id, _)| !acked.contains(id))
            .map(|(_, p)| p)
            .collect();
        n.replica = Replica::new(config(total, node, n.starts), n.durable.clone(), Timestamp(n.now))
            .map_err(|e| e.to_string())?;
        self.drain(node)?;
        // The client retries writes whose outcome it never learned, as new payloads.
        for p in lost {
            self.required.retain(|r| *r != p);
            let retry =
                Bytes::from(format!("{}/retry{}", String::from_utf8_lossy(&p), self.nodes[node].starts));
            let n = &mut self.nodes[node];
            let id = n.replica.propose(Timestamp(n.now), retry.clone()).map_err(|e| e.to_string())?;
            n.proposed.insert(id, retry.clone());
            self.proposals.push(retry.clone());
            self.required.push(retry);
            self.drain(node)?;
        }
        Ok(())
    }

    fn drain(&mut self, i: usize) -> Result<(), String> {
        let mut actions = Vec::new();
        self.nodes[i].replica.poll_actions(&mut actions);
        for a in actions {
            match a {
                Action::Send(o) => self.post(i, o.to.0 as usize, o.bytes),
                Action::Persist { id, records, then, replace } => {
                    let then = then.into_iter().map(|o| (o.to.0 as usize, o.bytes)).collect();
                    self.events.push_back(Event::Persisted { node: i, id, records, then, replace });
                }
                Action::Commit { slot, entries } => {
                    let n = &mut self.nodes[i];
                    if slot.0 != n.next_commit {
                        return Err(format!("n{i} committed slot {} out of order", slot.0));
                    }
                    n.next_commit += 1;
                    match self.log.get(&slot.0) {
                        Some(agreed) if *agreed != entries => {
                            return Err(format!(
                                "agreement broken at slot {}: {agreed:?} vs {entries:?}",
                                slot.0
                            ));
                        }
                        Some(_) => {}
                        None => {
                            for e in &entries {
                                if !self.proposals.contains(&e.payload) {
                                    return Err(format!("validity: {:?} was never proposed", e.payload));
                                }
                                if let Some(at) = self.committed_at.insert(e.payload.clone(), slot.0) {
                                    return Err(format!(
                                        "{:?} committed in slots {at} and {}",
                                        e.payload, slot.0
                                    ));
                                }
                            }
                            self.log.insert(slot.0, entries);
                        }
                    }
                }
                Action::Committed { id, slot } => {
                    // The exact proposal (origin, id, payload) must be logged in that slot.
                    let payload = self.nodes[i].proposed.get(&id);
                    let logged = payload.is_some_and(|p| {
                        self.log.get(&slot.0).is_some_and(|es| {
                            es.iter().any(|e| e.origin == NodeId(i as u32) && e.id == id && e.payload == *p)
                        })
                    });
                    self.nodes[i].acked.insert(id);
                    if !logged {
                        return Err(format!(
                            "n{i} reported Committed {id:?} at {slot:?} but it is not logged there"
                        ));
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Checks the protocol invariants on every replica, accumulating their evidence.
    fn observe(&mut self) -> Result<(), String> {
        let live: Vec<_> = self.nodes.iter().enumerate().map(|(i, n)| (i, n.replica.inspect())).collect();
        self.history.observe(self.nodes.len(), &live)
    }

    fn complete(&self) -> bool {
        self.required.iter().all(|p| self.committed_at.contains_key(p))
            && self.nodes.iter().all(|n| n.next_commit as usize == self.log.len())
    }
}

fn initial(cfg: &ModelConfig) -> Result<State, String> {
    let mut nodes = Vec::new();
    for i in 0..cfg.nodes {
        let replica = Replica::new(config(cfg.nodes, i, 0), [], Timestamp(0)).map_err(|e| e.to_string())?;
        nodes.push(MNode {
            replica,
            durable: Vec::new(),
            now: 0,
            next_commit: 0,
            starts: 0,
            proposed: BTreeMap::new(),
            acked: BTreeSet::new(),
        });
    }
    let mut init = State {
        nodes,
        events: VecDeque::new(),
        deviations: cfg.deviations,
        crashes: cfg.crashes,
        isolated: None,
        history: crate::invariants::History::default(),
        log: BTreeMap::new(),
        committed_at: BTreeMap::new(),
        proposals: Vec::new(),
        required: Vec::new(),
        depth: 0,
    };
    for i in 0..cfg.nodes {
        init.drain(i)?;
    }
    for (k, &i) in cfg.proposals.iter().enumerate() {
        let payload = Bytes::from(format!("w{k}@n{i}"));
        let id = init.nodes[i].replica.propose(Timestamp(0), payload.clone()).map_err(|e| e.to_string())?;
        init.nodes[i].proposed.insert(id, payload.clone());
        init.proposals.push(payload.clone());
        init.required.push(payload);
        init.drain(i)?;
    }
    Ok(init)
}

/// Explores the bounded schedule space; `Err` carries the violating schedule.
pub fn check(cfg: &ModelConfig) -> Result<ModelReport, String> {
    let init = initial(cfg)?;
    let mut report = ModelReport { exhausted: true, ..ModelReport::default() };
    let mut visited = HashSet::new();
    let mut stack: Vec<(State, Vec<Step>)> = vec![(init, Vec::new())];
    while let Some((state, path)) = stack.pop() {
        if !visited.insert(state.fingerprint()) {
            continue;
        }
        report.states += 1;
        if report.states >= cfg.max_states {
            report.exhausted = false;
            break;
        }
        if state.complete() {
            report.complete += 1;
        }
        if state.depth >= cfg.max_depth {
            // Liveness under the fair default schedule: a long path must still settle.
            if let Err(e) = settle(state.clone(), SETTLE_STEPS) {
                if std::env::var("QPX_MODEL_REPLAY").is_ok() {
                    replay(cfg, &path);
                }
                return Err(format!("{e}\nschedule ({} steps): {path:?}", path.len()));
            }
            report.depth_limited += 1;
            continue;
        }
        for (step, deviation) in state.steps() {
            let mut next = state.clone();
            if deviation {
                next.deviations -= 1;
            }
            let mut p = path.clone();
            p.push(step);
            let checked = next.apply(step).and_then(|()| next.observe());
            if let Err(e) = checked {
                return Err(format!("{e}\nschedule ({} steps): {p:?}", p.len()));
            }
            stack.push((next, p));
        }
    }
    Ok(report)
}

/// Default steps a depth-limited state may take to settle. QuePaxa terminates with
/// probability 1 (each leaderless round decides with probability > 1/2 under a
/// content-oblivious schedule), so long tails are expected but must end.
const SETTLE_STEPS: usize = 5_000;

/// Heals any partition, then follows the default schedule until nothing is pending (no
/// events, no timers): liveness once the network is whole again.
fn settle(mut st: State, max: usize) -> Result<(), String> {
    st.isolated = None;
    for _ in 0..max {
        if st.events.is_empty() && st.nodes.iter().all(|n| n.replica.next_deadline().is_none()) {
            return Ok(());
        }
        let Some((step, _)) =
            st.steps().into_iter().find(|(s, _)| matches!(s, Step::Run(0) | Step::Timeout(_) | Step::Heal))
        else {
            return Ok(());
        };
        st.apply(step)?;
        st.observe()?;
    }
    Err(format!("liveness: still busy after {max} default steps (log {} slots)", st.log.len()))
}

/// Debug aid (`QPX_MODEL_REPLAY=1`): re-runs `path`, printing what every step produced.
fn replay(cfg: &ModelConfig, path: &[Step]) {
    let mut st = initial(cfg).expect("initial state");
    for (k, step) in path.iter().enumerate() {
        let before = st.events.len();
        let _ = st.apply(*step);
        let new: Vec<String> = st
            .events
            .iter()
            .skip(before.saturating_sub(1))
            .map(|e| match e {
                Event::Deliver { to, bytes, .. } => {
                    format!("msg->{to} k{}", bytes.get(3).copied().unwrap_or(0))
                }
                Event::Persisted { node, records, .. } => {
                    let kinds: Vec<u8> = records.iter().map(|r| r.get(3).copied().unwrap_or(0)).collect();
                    format!("persist@{node} rec{kinds:?}")
                }
            })
            .collect();
        eprintln!("{k:3} {step:?} -> {new:?}");
    }
}

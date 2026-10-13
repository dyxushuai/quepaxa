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

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};

use bytes::Bytes;
use quepaxa::{Action, Config, Input, NodeId, PersistId, Replica, Timestamp};

/// Bounds of one exploration.
#[derive(Clone, Debug)]
pub struct ModelConfig {
    /// Cluster size.
    pub nodes: usize,
    /// One client write per entry, proposed at that replica at the start.
    pub proposals: Vec<usize>,
    /// Deviations from the default schedule along one path.
    pub deviations: u32,
    /// Crashes allowed along one path (each also costs a deviation).
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
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Event {
    Deliver { to: usize, bytes: Bytes },
    Persisted { node: usize, id: PersistId, records: Vec<Bytes>, then: Vec<(usize, Bytes)>, replace: bool },
}

#[derive(Clone)]
struct MNode {
    replica: Replica,
    durable: Vec<Bytes>,
    now: u64,
    next_commit: u64,
    starts: u64,
}

#[derive(Clone)]
struct State {
    nodes: Vec<MNode>,
    events: VecDeque<Event>,
    deviations: u32,
    crashes: u32,
    log: BTreeMap<u64, Vec<Bytes>>,
    depth: usize,
}

#[derive(Clone, Copy, Debug)]
enum Step {
    Run(usize),
    Lose(usize),
    Duplicate(usize),
    Timeout(usize),
    Crash { node: usize, keep: usize },
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
            (n.replica.fingerprint(), &n.durable, n.now, n.next_commit).hash(&mut h);
        }
        (&self.events, self.deviations, self.crashes).hash(&mut h);
        h.finish()
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
            (true, None) => {}
        }
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
        for i in 0..self.nodes.len() {
            if !self.events.is_empty() && self.nodes[i].replica.next_deadline().is_some() {
                steps.push((Step::Timeout(i), true));
            }
            if self.crashes > 0 {
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
                    steps.push((Step::Crash { node: i, keep }, true));
                }
            }
        }
        steps
    }

    fn apply(&mut self, step: Step) -> Result<(), String> {
        self.depth += 1;
        match step {
            Step::Run(k) => match self.events.remove(k).expect("offered event exists") {
                Event::Deliver { to, bytes } => {
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
                    self.events.extend(then.into_iter().map(|(to, bytes)| Event::Deliver { to, bytes }));
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
            Step::Crash { node, keep } => self.crash(node, keep),
        }
    }

    fn crash(&mut self, node: usize, keep: usize) -> Result<(), String> {
        self.crashes -= 1;
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
        n.replica = Replica::new(config(total, node, n.starts), n.durable.clone(), Timestamp(n.now))
            .map_err(|e| e.to_string())?;
        self.drain(node)
    }

    fn drain(&mut self, i: usize) -> Result<(), String> {
        let mut actions = Vec::new();
        self.nodes[i].replica.poll_actions(&mut actions);
        for a in actions {
            match a {
                Action::Send(o) => {
                    self.events.push_back(Event::Deliver { to: o.to.0 as usize, bytes: o.bytes })
                }
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
                    let payloads: Vec<Bytes> = entries.into_iter().map(|e| e.payload).collect();
                    match self.log.get(&slot.0) {
                        Some(agreed) if *agreed != payloads => {
                            return Err(format!(
                                "agreement broken at slot {}: {agreed:?} vs {payloads:?}",
                                slot.0
                            ));
                        }
                        Some(_) => {}
                        None => {
                            self.log.insert(slot.0, payloads);
                        }
                    }
                }
                Action::Committed { slot, .. } if !self.log.contains_key(&slot.0) => {
                    return Err(format!("n{i} reported Committed in unknown slot {}", slot.0));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn complete(&self, proposals: usize) -> bool {
        self.log.values().map(Vec::len).sum::<usize>() >= proposals
            && self.nodes.iter().all(|n| n.next_commit as usize == self.log.len())
    }
}

/// Explores the bounded schedule space; `Err` carries the violating schedule.
pub fn check(cfg: &ModelConfig) -> Result<ModelReport, String> {
    let mut nodes = Vec::new();
    for i in 0..cfg.nodes {
        let replica = Replica::new(config(cfg.nodes, i, 0), [], Timestamp(0)).map_err(|e| e.to_string())?;
        nodes.push(MNode { replica, durable: Vec::new(), now: 0, next_commit: 0, starts: 0 });
    }
    let mut init = State {
        nodes,
        events: VecDeque::new(),
        deviations: cfg.deviations,
        crashes: cfg.crashes,
        log: BTreeMap::new(),
        depth: 0,
    };
    for i in 0..cfg.nodes {
        init.drain(i)?;
    }
    for (k, &i) in cfg.proposals.iter().enumerate() {
        let payload = Bytes::from(format!("w{k}@n{i}"));
        init.nodes[i].replica.propose(Timestamp(0), payload).map_err(|e| e.to_string())?;
        init.drain(i)?;
    }
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
        if state.complete(cfg.proposals.len()) {
            report.complete += 1;
        }
        if state.depth >= cfg.max_depth {
            report.exhausted = false;
            continue;
        }
        for (step, deviation) in state.steps() {
            let mut next = state.clone();
            if deviation {
                next.deviations -= 1;
            }
            let mut p = path.clone();
            p.push(step);
            if let Err(e) = next.apply(step) {
                return Err(format!("{e}\nschedule ({} steps): {p:?}", p.len()));
            }
            stack.push((next, p));
        }
    }
    Ok(report)
}

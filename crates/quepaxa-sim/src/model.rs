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
use quepaxa::{Action, Config, Input, NodeId, PersistId, ProposalId, Replica, Timestamp};

/// Bounds of one exploration.
#[derive(Clone, Debug)]
pub struct ModelConfig {
    /// Cluster size.
    pub nodes: usize,
    /// One client write per entry, proposed at that replica at the start.
    pub proposals: Vec<usize>,
    /// One linearizable read per entry, requested at that replica once the first write is
    /// acknowledged: it must cover that write.
    pub reads: Vec<usize>,
    /// Deviations from the default schedule along one path.
    pub deviations: u32,
    /// Crash-restarts allowed along one path (a separate budget; they cost no deviation).
    pub crashes: u32,
    /// Longest path explored.
    pub max_depth: usize,
    /// Distinct states explored before stopping.
    pub max_states: usize,
    /// One client write per entry, proposed at that replica once every write in
    /// `proposals` is acknowledged.
    pub later: Vec<usize>,
    /// A replica cut off once every write in `proposals` is acknowledged, as if it crashed
    /// then (one partition, costing no deviation; healed like any other). A follower judges
    /// silence only against a member it has handed entries to before.
    pub silenced: Option<usize>,
    /// Network delay of every message, in nanoseconds: a delivery moves the receiver's
    /// clock to at least the send time plus this. Zero keeps time still between timers;
    /// silence and hedge delays are measured from these delays.
    pub link_delay: u64,
    /// A replica whose persists the default schedule completes only once nothing else is
    /// pending, no timer included: a disk slower than every timeout.
    pub slow_disk: Option<usize>,
    /// A replica whose messages the default schedule delivers only once nothing else is
    /// pending, no timer included: a leader slower than every timeout.
    pub slow_net: Option<usize>,
    /// Replicas beyond `nodes`, started outside the membership (to be added by a change).
    pub spares: usize,
    /// A membership change proposed at the start: at that replica, the new members (of
    /// replicas `0..nodes + spares`), preferred leaders by id.
    pub reconfigure: Option<(usize, Vec<usize>)>,
}

impl ModelConfig {
    /// A three-replica cluster with the given writes, no faults beyond the budgets.
    pub fn new(proposals: Vec<usize>, deviations: u32, crashes: u32) -> Self {
        Self {
            nodes: 3,
            proposals,
            reads: Vec::new(),
            deviations,
            crashes,
            max_depth: 200,
            max_states: 1_000_000,
            later: Vec::new(),
            silenced: None,
            link_delay: 0,
            slow_disk: None,
            slow_net: None,
            spares: 0,
            reconfigure: None,
        }
    }
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
    /// States in which every client write had committed and every read finished; a
    /// replica cut off may still be behind.
    pub served: usize,
    /// States in which some replica had hedged an entry (proposed it itself after its
    /// target was late or suspected).
    pub hedged: usize,
    /// States in which some replica had stepped aside.
    pub stepped_aside: usize,
    /// States in which an entry had committed in a batch proposed by a deputy: neither the
    /// leader (n0) nor the entry's origin.
    pub deputy: usize,
    /// States in which some replica had applied a membership change.
    pub reconfigured: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Event {
    Deliver { from: usize, to: usize, bytes: Bytes, at: u64 },
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
    /// Reads in progress: the slot each must cover.
    reads: BTreeMap<ProposalId, u64>,
}

#[derive(Clone)]
struct State {
    nodes: Vec<MNode>,
    events: VecDeque<Event>,
    deviations: u32,
    crashes: u32,
    isolated: Option<usize>,
    history: crate::invariants::History,
    ledger: crate::invariants::Ledger,
    /// Payloads that must eventually commit: client writes, minus those whose replica
    /// crashed before committing them (the client retries those as new payloads).
    required: Vec<Bytes>,
    /// Replicas still to request a read once a write is acknowledged.
    read_at: Vec<usize>,
    depth: usize,
    slow_disk: Option<usize>,
    slow_net: Option<usize>,
    later: Vec<usize>,
    silenced: Option<usize>,
    link_delay: u64,
    /// Writes in `proposals` not yet acknowledged; `later` and `silenced` wait for them.
    warmup: BTreeSet<Bytes>,
    /// An entry committed in a deputy's batch (not part of the fingerprint: coverage only).
    deputy: bool,
    /// Members of epoch 0 (replicas `0..members`; the others are spares).
    members: usize,
    /// Every applied epoch: first slot and members. All replicas must agree on them.
    epochs: BTreeMap<u64, (u64, Vec<u32>)>,
    /// The membership change to make (its members), and the proposal awaiting `Committed`
    /// and where (it is in no `Commit`).
    reconfigure: Option<Vec<NodeId>>,
    change: Option<(usize, ProposalId)>,
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
    // A spare (`i >= n`) runs with the original membership and waits to be added.
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
            (n.replica.fingerprint(), &n.durable, n.now, n.next_commit, &n.proposed, &n.acked, &n.reads)
                .hash(&mut h);
        }
        (
            &self.events,
            self.slow_disk,
            self.slow_net,
            &self.later,
            self.silenced,
            (&self.warmup, &self.epochs, self.change),
            self.deviations,
            self.crashes,
            self.isolated,
            &self.ledger,
            &self.history,
            &self.read_at,
        )
            .hash(&mut h);
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

    fn isolate(&mut self, node: usize) {
        self.isolated = Some(node);
        self.events.retain(|e| !matches!(e, Event::Deliver { from, to, .. } if *from == node || *to == node));
    }

    /// Proposes a client write at `i`.
    fn propose(&mut self, i: usize, payload: Bytes) -> Result<(), String> {
        let n = &mut self.nodes[i];
        let id = n.replica.propose(Timestamp(n.now), payload.clone()).map_err(|e| e.to_string())?;
        n.proposed.insert(id, payload.clone());
        self.ledger.proposed(payload.clone());
        self.required.push(payload);
        self.drain(i)
    }

    /// Queues a message unless either end is cut off.
    fn post(&mut self, from: usize, to: usize, bytes: Bytes) {
        if self.isolated != Some(from) && self.isolated != Some(to) {
            let at = if self.link_delay > 0 { self.nodes[from].now + self.link_delay } else { 0 };
            self.events.push_back(Event::Deliver { from, to, bytes, at });
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
        let slow = |e: &Event| match e {
            Event::Persisted { node, .. } => self.slow_disk == Some(*node),
            Event::Deliver { from, .. } => self.slow_net == Some(*from),
        };
        // The first event in arrival order that is not slow, else the first slow one once no
        // timer is left. Either is runnable: no earlier persist of its node precedes it (a
        // replica's persists are all slow or none are).
        let first = self.events.iter().position(|e| !slow(e));
        let mut steps = Vec::new();
        let default = match (first, earliest_timer()) {
            (Some(k), _) => Some(Step::Run(k)),
            (None, Some(i)) => Some(Step::Timeout(i)),
            (None, None) if !self.events.is_empty() => Some(Step::Run(0)),
            // A partition that outlives all other activity heals.
            (None, None) if self.isolated.is_some() => Some(Step::Heal),
            (None, None) => None,
        };
        let default_run = match default {
            Some(Step::Run(k)) => Some(k),
            _ => None,
        };
        let default_timer = match default {
            Some(Step::Timeout(i)) => Some(i),
            _ => None,
        };
        steps.extend(default.map(|s| (s, false)));
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
            if Some(k) != default_run && in_order {
                steps.push((Step::Run(k), true));
            }
            if matches!(e, Event::Deliver { .. }) {
                steps.push((Step::Lose(k), true));
                steps.push((Step::Duplicate(k), true));
            }
        }
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
                Event::Deliver { to, bytes, at, .. } => {
                    let n = &mut self.nodes[to];
                    n.now = n.now.max(at);
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
                let before = n.replica.fingerprint();
                n.replica.handle_timeout(Timestamp(n.now)).map_err(|e| e.to_string())?;
                self.drain(i)?;
                // A timer that changes nothing and stays due fires again at once, forever.
                let n = &self.nodes[i];
                match n.replica.next_deadline() {
                    Some(Timestamp(d)) if d <= n.now && n.replica.fingerprint() == before => Err(format!(
                        "timer storm: n{i} keeps firing its deadline {d} at {} without progress",
                        n.now
                    )),
                    _ => Ok(()),
                }
            }
            Step::Heal => {
                self.isolated = None;
                Ok(())
            }
            Step::Isolate(node) => {
                self.isolate(node);
                Ok(())
            }
            Step::Crash { node, keep } => self.crash(node, keep),
        }
    }

    fn crash(&mut self, node: usize, keep: usize) -> Result<(), String> {
        self.crashes -= 1;
        self.history.forget(self.members, node);
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
        let total = self.members;
        let n = &mut self.nodes[node];
        if let Some(records) = replaced {
            n.durable = records;
        }
        n.durable.extend(kept);
        n.starts += 1;
        n.next_commit = 0;
        let rechange = self.change.take_if(|(at, _)| *at == node).is_some();
        // Its clients' reads are lost with it.
        n.reads.clear();
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
        // The operator proposes the change again unless the replica still has it pending.
        if rechange {
            let members = self.reconfigure.clone().expect("a change was proposed");
            let n = &mut self.nodes[node];
            match n.replica.propose_reconfiguration(Timestamp(n.now), members, Vec::new()) {
                Ok(id) => self.change = Some((node, id)),
                Err(quepaxa::Error::ReconfigurationPending) => {}
                Err(e) => return Err(e.to_string()),
            }
            self.drain(node)?;
        }
        // The client retries writes whose outcome it never learned, as new payloads.
        for p in lost {
            self.required.retain(|r| *r != p);
            let retry =
                Bytes::from(format!("{}/retry{}", String::from_utf8_lossy(&p), self.nodes[node].starts));
            if self.warmup.remove(&p) {
                self.warmup.insert(retry.clone());
            }
            self.propose(node, retry)?;
        }
        Ok(())
    }

    fn drain(&mut self, i: usize) -> Result<(), String> {
        let mut actions = Vec::new();
        self.nodes[i].replica.poll_actions(&mut actions);
        let mut acked_slot = None;
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
                    // Checked at the first replica to commit the slot (inspecting is costly).
                    if !self.deputy && !entries.is_empty() && slot.0 as usize >= self.ledger.slots() {
                        let decided = n.replica.inspect().decided;
                        let by = decided.iter().find(|(s, _)| *s == slot.0).map(|(_, p)| p.origin);
                        self.deputy =
                            by.is_some_and(|by| by != 0 && entries.iter().any(|e| e.origin.0 != by));
                    }
                    self.ledger.commit(slot.0, &entries)?;
                }
                Action::Committed { id, .. } if self.change == Some((i, id)) => self.change = None,
                Action::Committed { id, slot } => {
                    self.nodes[i].acked.insert(id);
                    let payload = self.nodes[i].proposed.get(&id).cloned().unwrap_or_default();
                    self.warmup.remove(&payload);
                    self.ledger.acknowledged(NodeId(i as u32), id, &payload, slot.0, None)?;
                    acked_slot = Some(slot.0);
                }
                Action::Reconfigured { epoch, from, members } => {
                    let members: Vec<u32> = members.iter().map(|m| m.0).collect();
                    match self.epochs.get(&epoch) {
                        Some(seen) if *seen != (from.0, members.clone()) => {
                            return Err(format!(
                                "n{i} applied epoch {epoch} from {} as {members:?}; another replica: {seen:?}",
                                from.0
                            ));
                        }
                        Some(_) => {}
                        None => {
                            if let Some((other, _)) = self.epochs.iter().find(|(_, (f, _))| *f == from.0) {
                                return Err(format!(
                                    "n{i} applied epoch {epoch} from {}, where another replica began epoch {other}",
                                    from.0
                                ));
                            }
                            self.epochs.insert(epoch, (from.0, members));
                        }
                    }
                }
                Action::ReadReady { id, slot } => {
                    let n = &mut self.nodes[i];
                    if let Some(covers) = n.reads.remove(&id)
                        && (slot.0 < covers || n.next_commit <= slot.0)
                    {
                        return Err(format!(
                            "n{i} read ready at slot {} (applied up to {}) misses the write acknowledged in slot {covers}",
                            slot.0, n.next_commit
                        ));
                    }
                }
                _ => {}
            }
        }
        if acked_slot.is_some() && self.warmup.is_empty() {
            if let Some(node) = self.silenced.take() {
                self.isolate(node);
            }
            for (k, j) in std::mem::take(&mut self.later).into_iter().enumerate() {
                self.propose(j, Bytes::from(format!("later{k}@n{j}")))?;
            }
        }
        if let Some(covers) = acked_slot {
            for j in std::mem::take(&mut self.read_at) {
                let n = &mut self.nodes[j];
                let id = n.replica.read_barrier(Timestamp(n.now)).map_err(|e| e.to_string())?;
                n.reads.insert(id, covers);
                self.drain(j)?;
            }
        }
        Ok(())
    }

    /// Checks the protocol invariants on every replica, accumulating their evidence.
    fn observe(&mut self) -> Result<(), String> {
        let live: Vec<_> = self.nodes.iter().enumerate().map(|(i, n)| (i, n.replica.inspect())).collect();
        self.history.observe(self.members, &live)
    }

    /// Every client write committed somewhere and acknowledged to its client, every read
    /// issued and finished, and the membership change in effect and acknowledged.
    fn served(&self) -> bool {
        self.required.iter().all(|p| self.ledger.is_committed(p))
            && self.later.is_empty()
            && self.read_at.is_empty()
            && self
                .nodes
                .iter()
                .all(|n| n.reads.is_empty() && n.proposed.keys().all(|id| n.acked.contains(id)))
            && self.change.is_none()
            && (self.reconfigure.is_none() || !self.epochs.is_empty())
    }

    fn complete(&self) -> bool {
        self.served() && self.nodes.iter().all(|n| n.next_commit as usize == self.ledger.slots())
    }
}

fn initial(cfg: &ModelConfig) -> Result<State, String> {
    let mut nodes = Vec::new();
    for i in 0..cfg.nodes + cfg.spares {
        let replica = Replica::new(config(cfg.nodes, i, 0), [], Timestamp(0)).map_err(|e| e.to_string())?;
        nodes.push(MNode {
            replica,
            durable: Vec::new(),
            now: 0,
            next_commit: 0,
            starts: 0,
            proposed: BTreeMap::new(),
            acked: BTreeSet::new(),
            reads: BTreeMap::new(),
        });
    }
    let mut init = State {
        nodes,
        events: VecDeque::new(),
        deviations: cfg.deviations,
        crashes: cfg.crashes,
        isolated: None,
        history: crate::invariants::History::default(),
        ledger: crate::invariants::Ledger::default(),
        required: Vec::new(),
        read_at: cfg.reads.clone(),
        depth: 0,
        slow_disk: cfg.slow_disk,
        slow_net: cfg.slow_net,
        later: cfg.later.clone(),
        silenced: cfg.silenced,
        link_delay: cfg.link_delay,
        warmup: BTreeSet::new(),
        deputy: false,
        members: cfg.nodes,
        epochs: BTreeMap::new(),
        reconfigure: None,
        change: None,
    };
    for i in 0..init.nodes.len() {
        init.drain(i)?;
    }
    if let Some((at, members)) = &cfg.reconfigure {
        let n = &mut init.nodes[*at];
        let members: Vec<NodeId> = members.iter().map(|&m| NodeId(m as u32)).collect();
        let id = n
            .replica
            .propose_reconfiguration(Timestamp(0), members.clone(), Vec::new())
            .map_err(|e| e.to_string())?;
        init.reconfigure = Some(members);
        init.change = Some((*at, id));
        init.drain(*at)?;
    }
    for (k, &i) in cfg.proposals.iter().enumerate() {
        let payload = Bytes::from(format!("w{k}@n{i}"));
        init.warmup.insert(payload.clone());
        init.propose(i, payload)?;
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
        let metrics = || state.nodes.iter().map(|n| n.replica.metrics());
        report.hedged += usize::from(metrics().any(|m| m.hedged > 0));
        report.stepped_aside += usize::from(metrics().any(|m| m.stepped_aside > 0));
        report.deputy += usize::from(state.deputy);
        report.reconfigured += usize::from(!state.epochs.is_empty());
        if report.states >= cfg.max_states {
            report.exhausted = false;
            break;
        }
        if state.complete() {
            report.complete += 1;
        }
        report.served += usize::from(state.served());
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
        let steps = state.steps();
        // With nothing left to run, every client write must have committed somewhere and
        // every read finished. A replica that missed a decision may stay behind: it learns
        // it from the next message, and an idle cluster sends none.
        let idle = steps.first().is_none_or(|(s, d)| *d || matches!(s, Step::Crash { .. }));
        if idle && !state.served() {
            return Err(format!(
                "liveness: nothing left to run with client operations unfinished (log {} slots)\nschedule ({} steps): {path:?}",
                state.ledger.slots(),
                path.len()
            ));
        }
        for (step, deviation) in steps {
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

/// Heals any partition and slow disk or link, then follows the default schedule until
/// nothing is pending (no events, no timers): liveness once the network is whole again.
fn settle(mut st: State, max: usize) -> Result<(), String> {
    st.isolated = None;
    st.silenced = None;
    st.slow_disk = None;
    st.slow_net = None;
    for _ in 0..max {
        if st.events.is_empty() && st.nodes.iter().all(|n| n.replica.next_deadline().is_none()) {
            return if st.served() {
                Ok(())
            } else {
                Err(format!(
                    "liveness: settled with client operations unfinished (log {} slots)",
                    st.ledger.slots()
                ))
            };
        }
        // The default step comes first; a crash is never one.
        let Some((step, _)) = st.steps().into_iter().next().filter(|(s, _)| !matches!(s, Step::Crash { .. }))
        else {
            return Ok(());
        };
        st.apply(step)?;
        st.observe()?;
    }
    Err(format!("liveness: still busy after {max} default steps (log {} slots)", st.ledger.slots()))
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

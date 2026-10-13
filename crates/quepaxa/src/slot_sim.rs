//! Deterministic single-slot simulation: proposers and recorders over a seeded faulty
//! network, with the paper's global invariants G1–G5 checked after every event.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use bytes::Bytes;

use crate::codec::{Body, DecodeError, Envelope, Message, WireCtx, reply_proposal};
use crate::proposal::{LEADER_PRIORITY, Proposal, ValueKey};
use crate::proposer::{Progress, Reply, SlotProposer};
use crate::recorder::Register;
use crate::rng::ChaCha20;
use crate::types::{NodeId, Slot, Step};

const CTX: WireCtx = WireCtx { cluster_id: [1; 16], config_epoch: 0, max_message_bytes: 1 << 16 };
const SLOT: Slot = Slot(0);

#[derive(Clone, Copy, Debug)]
pub(crate) struct Faults {
    /// Per-mille chance a sent message is lost.
    pub drop: u32,
    /// Per-mille chance a sent message is duplicated.
    pub dup: u32,
    /// Per-mille chance per event that the network partitions (until healed).
    pub partition: u32,
}

pub(crate) struct Sim {
    rng: ChaCha20,
    faults: Faults,
    regs: Vec<Register>,
    props: BTreeMap<usize, SlotProposer>,
    inputs: BTreeSet<ValueKey>,
    /// In-flight encoded messages.
    net: Vec<Bytes>,
    /// Nodes cut off from everyone else while a partition is active.
    cut: BTreeSet<usize>,
    /// Decided value and the step it was decided at, per proposer.
    decisions: BTreeMap<usize, (Proposal, Step)>,
    pub trace: String,
}

fn ids(n: usize) -> Vec<NodeId> {
    (0..n as u32).map(NodeId).collect()
}

fn value(origin: usize) -> Proposal {
    let key = ValueKey { origin: NodeId(origin as u32), incarnation: 1, seq: 0 };
    Proposal { priority: 0, key, value: Bytes::from(format!("value-{origin}")) }
}

impl Sim {
    /// `n` replicas; the first `k` run proposers, node 0 being the leader.
    pub fn new(seed: [u8; 32], n: usize, k: usize, faults: Faults) -> Self {
        let mut rng = ChaCha20::new(seed);
        let props: BTreeMap<usize, SlotProposer> =
            (0..k).map(|i| (i, SlotProposer::new(ids(n), i == 0, value(i), &mut rng))).collect();
        let inputs = (0..k).map(|i| value(i).key).collect();
        let mut sim = Self {
            rng,
            faults,
            regs: alloc::vec![Register::default(); n],
            props,
            inputs,
            net: Vec::new(),
            cut: BTreeSet::new(),
            decisions: BTreeMap::new(),
            trace: String::new(),
        };
        for i in 0..k {
            sim.send_requests(i);
        }
        sim
    }

    fn chance(&mut self, per_mille: u32) -> bool {
        self.rng.next_u32() % 1000 < per_mille
    }

    fn pick(&mut self, n: usize) -> usize {
        (self.rng.next_u64() % n as u64) as usize
    }

    fn env(from: usize, to: usize) -> Envelope {
        Envelope {
            cluster_id: CTX.cluster_id,
            config_epoch: 0,
            from: NodeId(from as u32),
            to: NodeId(to as u32),
            commit_index: 0,
        }
    }

    fn send(&mut self, from: usize, to: usize, body: Body) {
        if self.chance(self.faults.drop) {
            return;
        }
        let bytes = Message { env: Self::env(from, to), body }.encode();
        if self.chance(self.faults.dup) {
            self.net.push(bytes.clone());
        }
        self.net.push(bytes);
    }

    fn send_requests(&mut self, i: usize) {
        let Some(p) = self.props.get(&i) else { return };
        if p.decided().is_some() {
            return;
        }
        let step = p.step();
        let reqs: Vec<(NodeId, Proposal)> = p.requests().map(|(j, q)| (j, q.clone())).collect();
        for (j, proposal) in reqs {
            self.send(i, j.0 as usize, Body::RecordRequest { slot: SLOT, step, proposal });
        }
    }

    fn reachable(&self, a: usize, b: usize) -> bool {
        a == b || self.cut.contains(&a) == self.cut.contains(&b)
    }

    /// Runs one scheduler event. Returns false once nothing is left to do.
    pub fn event(&mut self) -> bool {
        if self.props.values().all(|p| p.decided().is_some()) {
            return false;
        }
        if self.cut.is_empty() && self.chance(self.faults.partition) {
            let n = self.regs.len();
            let minority = (n - 1) / 2;
            while self.cut.len() < minority.max(1) {
                let x = self.pick(n);
                self.cut.insert(x);
            }
            self.trace += &format!("partition {:?}\n", self.cut);
        } else if !self.cut.is_empty() && self.chance(50) {
            self.cut.clear();
            self.trace += "heal\n";
        }
        // Retransmission timer firing on a random proposer.
        if self.net.is_empty() || self.chance(30) {
            let ids: Vec<usize> = self.props.keys().copied().collect();
            let i = ids[self.pick(ids.len())];
            self.send_requests(i);
            return true;
        }
        let idx = self.pick(self.net.len());
        let bytes = self.net.swap_remove(idx);
        let msg = Message::decode(&bytes, &CTX).expect("sim only sends valid messages");
        let (from, to) = (msg.env.from.0 as usize, msg.env.to.0 as usize);
        if !self.reachable(from, to) {
            self.net.push(bytes); // held until heal
            return true;
        }
        self.deliver(from, to, msg.body);
        self.check_invariants();
        true
    }

    fn deliver(&mut self, from: usize, to: usize, body: Body) {
        match body {
            Body::RecordRequest { step, proposal, .. } => {
                let key = proposal.key;
                self.regs[to].record(step, proposal);
                let s = self.regs[to].summary();
                let first = s.first.as_ref().map(|p| reply_proposal(p, key));
                let prev_agg = s.prev_agg.as_ref().map(|p| reply_proposal(p, key));
                self.trace += &format!("{from}->{to} rec s{step} => S{}\n", s.step);
                self.send(
                    to,
                    from,
                    Body::RecordReply { slot: SLOT, req_step: step, step: s.step, first, prev_agg },
                );
            }
            Body::RecordReply { req_step, step, first, prev_agg, .. } => {
                let Some(p) = self.props.get_mut(&to) else {
                    return;
                };
                let Some(request) = p.request_for(NodeId(from as u32)).cloned() else {
                    return;
                };
                if req_step != p.step() {
                    return; // the request it answers is superseded; elided values may not resolve
                }
                let resolve = |w: Option<crate::codec::WireProposal>| match w {
                    None => Ok(None),
                    Some(w) => w.resolve(&request).map(Some).ok_or(DecodeError::BadTag),
                };
                let (Ok(first), Ok(prev_agg)) = (resolve(first), resolve(prev_agg)) else {
                    panic!("recorder elided a value the proposer cannot hold")
                };
                let step_before = p.step();
                match p.on_reply(
                    NodeId(from as u32),
                    Reply { req_step, step, first, prev_agg },
                    &mut self.rng,
                ) {
                    Progress::Waiting => {}
                    Progress::Advanced => {
                        self.trace += &format!("p{to} -> s{}\n", self.props[&to].step());
                        self.send_requests(to);
                    }
                    Progress::Decided(v) => {
                        self.trace += &format!("p{to} decided {:?} at s{step_before}\n", v.key);
                        self.decisions.insert(to, (v, step_before));
                    }
                }
            }
            _ => unreachable!("the single-slot sim only sends record messages"),
        }
    }

    fn check_invariants(&self) {
        let decided: Vec<&(Proposal, Step)> = self.decisions.values().collect();
        if let Some((first, _)) = decided.first() {
            // G1 agreement and G2 validity.
            assert!(decided.iter().all(|(v, _)| v.key == first.key), "G1 violated\n{}", self.trace);
            assert!(self.inputs.contains(&first.key), "G2 violated\n{}", self.trace);
            // G3: once decided in round r, recorder state from round r+1 on carries v.
            let r = decided.iter().map(|(_, s)| s / 4).min().unwrap_or(1);
            // `prev_agg` holds the previous step's aggregate, so it is bound one step later.
            for reg in &self.regs {
                let locked = 4 * (r + 1);
                let cur = [&reg.first, &reg.agg].into_iter().filter(|_| reg.step >= locked);
                let prev = [&reg.prev_agg].into_iter().filter(|_| reg.step > locked);
                for p in cur.chain(prev).flatten() {
                    assert_eq!(p.key, first.key, "G3 violated\n{}", self.trace);
                }
            }
        }
        // G4: a majority holding the leader's proposal first at step 4 locks the value.
        let n = self.regs.len();
        let locked = self
            .regs
            .iter()
            .filter(|r| r.step == 4 && r.first.as_ref().is_some_and(|f| f.priority == LEADER_PRIORITY))
            .count();
        if locked > n / 2 {
            for (v, _) in &decided {
                assert_eq!(v.key, value(0).key, "G4 violated\n{}", self.trace);
            }
        }
        // G5: at most one distinct proposal ever carries H.
        let h: BTreeSet<ValueKey> = self
            .regs
            .iter()
            .flat_map(|r| [&r.first, &r.agg, &r.prev_agg])
            .flatten()
            .filter(|p| p.priority == LEADER_PRIORITY)
            .map(|p| p.key)
            .collect();
        assert!(h.len() <= 1, "G5 violated\n{}", self.trace);
    }

    pub fn heal_and_stop_faults(&mut self) {
        self.cut.clear();
        self.faults = Faults { drop: 0, dup: 0, partition: 0 };
    }

    pub fn all_decided(&self) -> bool {
        self.props.keys().all(|i| self.decisions.contains_key(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(i: u64) -> [u8; 32] {
        let mut s = [0u8; 32];
        s[..8].copy_from_slice(&i.to_le_bytes());
        s
    }

    fn run(seed: [u8; 32], n: usize, k: usize, faults: Faults) -> Sim {
        let mut sim = Sim::new(seed, n, k, faults);
        for _ in 0..3_000 {
            if !sim.event() {
                break;
            }
        }
        // Liveness: once faults stop and the network heals, every proposer decides.
        sim.heal_and_stop_faults();
        for _ in 0..20_000 {
            if !sim.event() {
                break;
            }
        }
        assert!(sim.all_decided(), "no progress after heal\n{}", sim.trace);
        sim
    }

    const HOSTILE: Faults = Faults { drop: 100, dup: 50, partition: 5 };

    #[test]
    fn single_slot_is_safe_and_live_across_1000_seeds() {
        for i in 0..1000 {
            let (n, k) = [(3, 1), (3, 2), (3, 3), (5, 2), (5, 5)][(i % 5) as usize];
            run(seed(i), n, k, HOSTILE);
        }
    }

    #[test]
    fn same_seed_reproduces_the_same_trace() {
        let a = run(seed(42), 5, 3, HOSTILE).trace;
        let b = run(seed(42), 5, 3, HOSTILE).trace;
        assert_eq!(a, b);
    }

    #[test]
    fn fault_free_leader_decides_its_value_on_the_fast_path() {
        let sim = run(seed(7), 3, 1, Faults { drop: 0, dup: 0, partition: 0 });
        let (v, step) = &sim.decisions[&0];
        assert_eq!((v.key, *step), (value(0).key, 4));
    }
}

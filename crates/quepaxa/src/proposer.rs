//! Proposer for one slot (paper Alg. 4).
//!
//! Drives `record` requests through the step clock until it learns a decision. It holds
//! no durable state: safety comes from the recorders, so a restarted proposer may start
//! a slot afresh (except for the leader priority, which the replica guards).

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::proposal::{LEADER_PRIORITY, Proposal};
use crate::rng::ChaCha20;
use crate::types::{NodeId, Step};

const FIRST_STEP: Step = 4;

/// A recorder's answer to this proposer's request at `req_step`, values already resolved.
#[derive(Clone, Debug)]
pub(crate) struct Reply {
    pub req_step: Step,
    pub step: Step,
    pub first: Option<Proposal>,
    pub prev_agg: Option<Proposal>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Progress {
    /// Nothing new to send.
    Waiting,
    /// Moved to a new step: send [`SlotProposer::requests`] to every recorder.
    Advanced,
    /// The slot is decided with this value.
    Decided(Proposal),
}

pub(crate) struct SlotProposer {
    recorders: Vec<NodeId>,
    quorum: usize,
    /// Whether this proposer is the slot's agreed leader (may use `H` at the first step).
    leader: bool,
    step: Step,
    /// Working proposal `p`.
    p: Proposal,
    /// Per-recorder proposals `p_j` sent at the current step.
    sent: BTreeMap<NodeId, Proposal>,
    /// Same-step replies at the current step, one per recorder.
    replies: BTreeMap<NodeId, Reply>,
    decided: Option<Proposal>,
}

impl SlotProposer {
    /// Starts at round 1, phase 0 proposing `value` (its priority is chosen here).
    pub fn new(recorders: Vec<NodeId>, leader: bool, value: Proposal, rng: &mut ChaCha20) -> Self {
        let quorum = recorders.len() / 2 + 1;
        let p = Proposal {
            priority: LEADER_PRIORITY,
            ..value
        };
        let mut s = Self {
            recorders,
            quorum,
            leader,
            step: FIRST_STEP,
            p,
            sent: BTreeMap::new(),
            replies: BTreeMap::new(),
            decided: None,
        };
        s.prepare(rng);
        s
    }

    pub fn step(&self) -> Step {
        self.step
    }

    #[cfg(test)]
    pub fn decided(&self) -> Option<&Proposal> {
        self.decided.as_ref()
    }

    /// The request for recorder `j` at the current step.
    pub fn request_for(&self, j: NodeId) -> Option<&Proposal> {
        self.sent.get(&j)
    }

    /// Current-step requests not yet answered (for first send and retransmission).
    pub fn requests(&self) -> impl Iterator<Item = (NodeId, &Proposal)> {
        self.sent
            .iter()
            .filter(|(j, _)| !self.replies.contains_key(j))
            .map(|(j, p)| (*j, p))
    }

    /// Paper Alg. 4 loop head: the per-recorder proposals for the current step.
    fn prepare(&mut self, rng: &mut ChaCha20) {
        let randomize = self.step.is_multiple_of(4) && (self.step > FIRST_STEP || !self.leader);
        self.sent = self
            .recorders
            .iter()
            .map(|&j| {
                let priority = if randomize {
                    rng.priority()
                } else {
                    self.p.priority
                };
                (
                    j,
                    Proposal {
                        priority,
                        ..self.p.clone()
                    },
                )
            })
            .collect();
        self.replies.clear();
        // Later phases may carry an adopted leader proposal, so only phase 0 is checked.
        debug_assert!(
            !self.step.is_multiple_of(4)
                || self
                    .sent
                    .values()
                    .all(|q| q.priority != LEADER_PRIORITY
                        || (self.leader && self.step == FIRST_STEP)),
            "P3: phase-0 H only from the leader at the first step"
        );
    }

    fn advance(&mut self, step: Step, rng: &mut ChaCha20) -> Progress {
        debug_assert!(step > self.step, "P2: step strictly increases");
        self.step = step;
        self.prepare(rng);
        Progress::Advanced
    }

    pub fn on_reply(&mut self, from: NodeId, reply: Reply, rng: &mut ChaCha20) -> Progress {
        if self.decided.is_some() || reply.req_step != self.step || !self.sent.contains_key(&from) {
            return Progress::Waiting;
        }
        debug_assert!(
            reply.step >= reply.req_step,
            "P1: recorder replied with an older step"
        );
        if reply.step > self.step {
            // Catch up to the recorder's step, adopting its first value there (A12: take
            // the first reply that is ahead; any such reply is valid by Lemma C.3).
            let Some(first) = reply.first else {
                return Progress::Waiting;
            };
            self.p = first;
            return self.advance(reply.step, rng);
        }
        if reply.step < self.step {
            return Progress::Waiting;
        }
        self.replies.insert(from, reply);
        if self.replies.len() < self.quorum {
            return Progress::Waiting;
        }
        let firsts = || self.replies.values().map(|r| r.first.as_ref());
        let best_agg = self
            .replies
            .values()
            .filter_map(|r| r.prev_agg.as_ref())
            .max()
            .cloned();
        match self.step % 4 {
            0 => {
                if self.step == FIRST_STEP
                    && firsts().all(|f| f.is_some_and(|f| f.priority == LEADER_PRIORITY))
                {
                    let v = firsts().flatten().next().cloned();
                    return self.decide(v);
                }
                if let Some(best) = firsts().flatten().max().cloned() {
                    self.p = best;
                }
            }
            2 => {
                if best_agg.as_ref() == Some(&self.p) {
                    let p = self.p.clone();
                    return self.decide(Some(p));
                }
            }
            3 => match best_agg {
                Some(best) => self.p = best,
                None => debug_assert!(false, "P6: phase 3 quorum must carry an aggregate"),
            },
            _ => {}
        }
        self.advance(self.step + 1, rng)
    }

    fn decide(&mut self, v: Option<Proposal>) -> Progress {
        let Some(v) = v else { return Progress::Waiting };
        debug_assert!(self.decided.is_none(), "P5: decide once");
        self.decided = Some(v.clone());
        Progress::Decided(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proposal::tests::prop;
    use crate::recorder::Register;
    use alloc::vec;
    use proptest::prelude::*;

    fn ids(n: u32) -> Vec<NodeId> {
        (0..n).map(NodeId).collect()
    }

    fn rng() -> ChaCha20 {
        ChaCha20::new([5; 32])
    }

    /// Delivers proposer `pi`'s request for recorder `j` and its reply.
    fn exchange(
        ps: &mut [SlotProposer],
        regs: &mut [Register],
        pi: usize,
        j: usize,
        rng: &mut ChaCha20,
    ) -> Progress {
        let Some(req) = ps[pi].request_for(NodeId(j as u32)).cloned() else {
            return Progress::Waiting;
        };
        let req_step = ps[pi].step();
        regs[j].record(req_step, req);
        let s = regs[j].summary();
        let reply = Reply {
            req_step,
            step: s.step,
            first: s.first,
            prev_agg: s.prev_agg,
        };
        ps[pi].on_reply(NodeId(j as u32), reply, rng)
    }

    fn run_alone(p: &mut SlotProposer, regs: &mut [Register], rng: &mut ChaCha20) -> Proposal {
        for _ in 0..64 {
            for j in 0..regs.len() {
                if let Progress::Decided(v) = exchange(core::slice::from_mut(p), regs, 0, j, rng) {
                    return v;
                }
            }
        }
        panic!("no decision")
    }

    #[test]
    fn leader_alone_decides_on_fast_path_at_first_step() {
        let mut rng = rng();
        let mut regs = vec![Register::default(); 3];
        let mut p = SlotProposer::new(ids(3), true, prop(0, 1, 0), &mut rng);
        assert_eq!(
            exchange(core::slice::from_mut(&mut p), &mut regs, 0, 0, &mut rng),
            Progress::Waiting
        );
        let Progress::Decided(v) =
            exchange(core::slice::from_mut(&mut p), &mut regs, 0, 1, &mut rng)
        else {
            panic!()
        };
        assert_eq!(
            (v.key, v.priority, p.step()),
            (prop(0, 1, 0).key, LEADER_PRIORITY, 4)
        );
    }

    #[test]
    fn non_leader_alone_decides_its_value_at_round_one_phase_two() {
        let mut rng = rng();
        let mut regs = vec![Register::default(); 3];
        let mut p = SlotProposer::new(ids(3), false, prop(0, 2, 0), &mut rng);
        let v = run_alone(&mut p, &mut regs, &mut rng);
        assert_eq!((v.key, p.step()), (prop(0, 2, 0).key, 6));
    }

    #[test]
    fn non_leader_sends_a_distinct_random_priority_to_each_recorder() {
        let p = SlotProposer::new(ids(3), false, prop(0, 2, 0), &mut rng());
        let prios: Vec<u64> = p.requests().map(|(_, q)| q.priority).collect();
        assert!(prios[0] != prios[1] && prios[1] != prios[2] && !prios.contains(&LEADER_PRIORITY));
    }

    #[test]
    fn phase_zero_adopts_the_best_first_value_of_the_quorum() {
        let mut rng = rng();
        let mut p = SlotProposer::new(ids(3), false, prop(0, 2, 0), &mut rng);
        let reply = |first| Reply {
            req_step: 4,
            step: 4,
            first: Some(first),
            prev_agg: None,
        };
        p.on_reply(NodeId(0), reply(prop(3, 7, 0)), &mut rng);
        assert_eq!(
            p.on_reply(NodeId(1), reply(prop(9, 8, 0)), &mut rng),
            Progress::Advanced
        );
        assert!(p.requests().all(|(_, q)| *q == prop(9, 8, 0)));
    }

    fn feed(
        p: &mut SlotProposer,
        rng: &mut ChaCha20,
        step: Step,
        first: Proposal,
        agg: Option<Proposal>,
    ) -> Progress {
        let r = |f: Proposal| Reply {
            req_step: step,
            step,
            first: Some(f),
            prev_agg: agg.clone(),
        };
        p.on_reply(NodeId(0), r(first.clone()), rng);
        p.on_reply(NodeId(1), r(first), rng)
    }

    #[test]
    fn phase_two_without_matching_aggregate_continues_to_phase_three() {
        let mut rng = rng();
        let mut p = SlotProposer::new(ids(3), false, prop(0, 2, 0), &mut rng);
        feed(&mut p, &mut rng, 4, prop(5, 2, 0), None);
        feed(&mut p, &mut rng, 5, prop(5, 2, 0), Some(prop(5, 2, 0)));
        assert_eq!(
            feed(&mut p, &mut rng, 6, prop(5, 2, 0), Some(prop(8, 3, 0))),
            Progress::Advanced
        );
        assert_eq!(p.step(), 7);
        assert_eq!(
            feed(&mut p, &mut rng, 7, prop(5, 2, 0), Some(prop(8, 3, 0))),
            Progress::Advanced
        );
        assert!(
            p.requests().all(|(_, q)| q.key == prop(8, 3, 0).key),
            "phase 3 adopts best aggregate"
        );
    }

    #[test]
    fn reply_from_a_recorder_ahead_makes_the_proposer_catch_up() {
        let mut rng = rng();
        let mut p = SlotProposer::new(ids(3), false, prop(0, 2, 0), &mut rng);
        let reply = Reply {
            req_step: 4,
            step: 9,
            first: Some(prop(4, 6, 0)),
            prev_agg: None,
        };
        assert_eq!(p.on_reply(NodeId(2), reply, &mut rng), Progress::Advanced);
        assert_eq!(p.step(), 9);
        assert!(p.requests().all(|(_, q)| *q == prop(4, 6, 0)));
    }

    #[test]
    fn leader_catching_up_at_the_first_step_keeps_the_adopted_priority() {
        // Ambiguity A14: the leader must not stamp H onto another proposer's value.
        let mut rng = rng();
        let mut p = SlotProposer::new(ids(3), true, prop(0, 1, 0), &mut rng);
        let reply = Reply {
            req_step: 4,
            step: 4,
            first: Some(prop(4, 6, 0)),
            prev_agg: None,
        };
        p.on_reply(NodeId(0), reply.clone(), &mut rng);
        p.on_reply(NodeId(1), reply, &mut rng);
        assert!(
            p.requests()
                .all(|(_, q)| q.priority != LEADER_PRIORITY && q.key == prop(4, 6, 0).key)
        );
    }

    #[test]
    fn replies_to_older_requests_are_ignored() {
        let mut rng = rng();
        let mut p = SlotProposer::new(ids(3), false, prop(0, 2, 0), &mut rng);
        let stale = Reply {
            req_step: 3,
            step: 9,
            first: Some(prop(4, 6, 0)),
            prev_agg: None,
        };
        assert_eq!(p.on_reply(NodeId(0), stale, &mut rng), Progress::Waiting);
        assert_eq!(p.step(), 4);
    }

    // Several proposers race over shared recorders in a random delivery order: every
    // decision agrees (G1) and is some proposer's input (G2).
    proptest! {
        #[test]
        fn racing_proposers_agree(n_props in 1usize..4, n_recs in prop::sample::select(vec![3usize, 5]),
                                  seed: [u8; 32], order in prop::collection::vec((0usize..4, 0usize..5), 0..400)) {
            let mut rng = ChaCha20::new(seed);
            let mut regs = vec![Register::default(); n_recs];
            let mut ps: Vec<SlotProposer> = (0..n_props)
                .map(|i| SlotProposer::new(ids(n_recs as u32), i == 0, prop(0, i as u32, 0), &mut rng))
                .collect();
            for (pi, j) in order.into_iter().chain((0..2000).map(|k| (k % 4, (k / 4) % 5))) {
                let (pi, j) = (pi % n_props, j % n_recs);
                if ps[pi].decided().is_none() {
                    exchange(&mut ps, &mut regs, pi, j, &mut rng);
                }
                if ps.iter().all(|p| p.decided().is_some()) {
                    break;
                }
            }
            let decided: Vec<&Proposal> = ps.iter().filter_map(SlotProposer::decided).collect();
            prop_assert!(!decided.is_empty(), "liveness under fair delivery");
            for d in &decided {
                prop_assert_eq!(d.key, decided[0].key, "G1 agreement");
                prop_assert!((d.key.origin.0 as usize) < n_props, "G2 validity");
            }
        }
    }
}

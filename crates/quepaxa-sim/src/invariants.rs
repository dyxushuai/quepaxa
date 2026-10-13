//! The paper's global invariants checked across real replicas (spec §9; paper deep read
//! §10), on protocol-internal state exposed by `Replica::inspect` (feature `model`).
//!
//! - **G1 agreement**: a slot is only ever decided to one value.
//! - **G3 value lock**: once some proposer decided `v` in round `r`, every value stored at a
//!   step `≥ 4(r+1)` (and every previous-step aggregate above it) carries `v`.
//! - **G4 fast-path lock**: once a majority of recorders held the leader's top-priority
//!   proposal as the first value of step 4, the slot can only decide that value.
//! - **G5 unique H**: at most one distinct value ever carries the leader priority per slot.
//!
//! [`Ledger`] checks what clients see: agreement and validity of committed entries, each
//! payload committed at most once, and `Committed` only for the exact proposal in the log.
//!
//! Registers move on and are discarded after a durable decision, so the evidence behind
//! G1, G4 and G5 is accumulated in a [`History`] rather than re-derived from the current
//! state alone. Evidence remembers which replicas showed it: a crash discards what only the
//! crashed replica's volatile state showed (it never had an effect: replies wait for
//! durability), while durable state is observed again after recovery.

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use quepaxa::inspect::{Inspection, ProposalView};
use quepaxa::{Entry, NodeId, ProposalId};

type Value = (u32, u64, u64, u64);

/// Evidence observed so far in one execution.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct History {
    decided: BTreeMap<u64, Value>,
    lock_step: BTreeMap<u64, u32>,
    /// Leader-priority values per slot, with the replicas whose registers showed them.
    leader_values: BTreeMap<u64, BTreeMap<Value, BTreeSet<usize>>>,
    /// Step-4 fast-path locks per slot, with the replicas forming the majority.
    fast_locked: BTreeMap<u64, (Value, BTreeSet<usize>)>,
}

impl History {
    /// A replica crashed: evidence only its volatile state showed is gone.
    pub fn forget(&mut self, n: usize, replica: usize) {
        for values in self.leader_values.values_mut() {
            for seen in values.values_mut() {
                seen.remove(&replica);
            }
            values.retain(|_, seen| !seen.is_empty());
        }
        for (_, seen) in self.fast_locked.values_mut() {
            seen.remove(&replica);
        }
        self.fast_locked.retain(|_, (_, seen)| seen.len() > n / 2);
    }

    /// Folds the live replicas' state into the history and checks G1, G3, G4 and G5.
    pub fn observe(&mut self, n: usize, replicas: &[(usize, Inspection)]) -> Result<(), String> {
        for (_, ins) in replicas {
            let decided = ins.decided.iter().map(|(s, v)| (*s, v, None));
            let here = ins.decided_here.iter().map(|(s, v, step)| (*s, v, Some(*step)));
            for (slot, v, step) in decided.chain(here) {
                match self.decided.get(&slot) {
                    Some(d) if *d != v.value() => {
                        return Err(format!("G1 agreement: slot {slot} decided {d:?} and {:?}", v.value()));
                    }
                    Some(_) => {}
                    None => {
                        self.decided.insert(slot, v.value());
                    }
                }
                if let Some(step) = step {
                    let lock = 4 * (step / 4 + 1);
                    self.lock_step.entry(slot).and_modify(|l| *l = (*l).min(lock)).or_insert(lock);
                }
            }
        }
        let mut step4_firsts: BTreeMap<u64, BTreeMap<Value, BTreeSet<usize>>> = BTreeMap::new();
        for (i, ins) in replicas {
            for (slot, reg) in &ins.registers {
                for p in [reg.first, reg.agg, reg.prev_agg].into_iter().flatten() {
                    if p.priority == u64::MAX {
                        self.leader_values.entry(*slot).or_default().entry(p.value()).or_default().insert(*i);
                    }
                }
                if reg.step == 4
                    && let Some(f) = reg.first.filter(|f| f.priority == u64::MAX)
                {
                    step4_firsts.entry(*slot).or_default().entry(f.value()).or_default().insert(*i);
                }
                if let (Some(lock), Some(v)) = (self.lock_step.get(slot), self.decided.get(slot)) {
                    let current = [reg.first, reg.agg].into_iter().filter(|_| reg.step >= *lock);
                    let previous = [reg.prev_agg].into_iter().filter(|_| reg.step > *lock);
                    for p in current.chain(previous).flatten() {
                        if p.value() != *v {
                            return Err(format!(
                                "G3 value lock: n{i} slot {slot} step {} holds {}, decided {v:?} locks from step {lock}",
                                reg.step,
                                show(&p)
                            ));
                        }
                    }
                }
            }
        }
        for (slot, holders) in step4_firsts {
            for (v, seen) in holders {
                if seen.len() > n / 2 {
                    self.fast_locked.entry(slot).or_insert((v, seen));
                }
            }
        }
        for (slot, values) in &self.leader_values {
            if values.len() > 1 {
                let values: Vec<_> = values.iter().collect();
                return Err(format!(
                    "G5 unique H: slot {slot} had leader-priority values (value, seen at) {values:?}"
                ));
            }
        }
        for (slot, (v, _)) in &self.fast_locked {
            if let Some(d) = self.decided.get(slot)
                && d != v
            {
                return Err(format!(
                    "G4 fast-path lock: slot {slot} was locked to {v:?} by a majority but decided {d:?}"
                ));
            }
        }
        Ok(())
    }
}

/// The agreed log as clients observe it, built from every replica's commits.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct Ledger {
    proposed: BTreeSet<Bytes>,
    log: BTreeMap<u64, Vec<Entry>>,
    committed_at: BTreeMap<Bytes, u64>,
}

impl Ledger {
    /// A client proposed `payload`; payloads must be unique per execution.
    pub fn proposed(&mut self, payload: Bytes) {
        self.proposed.insert(payload);
    }

    /// A replica committed `entries` in `slot`; returns whether the slot is new to the log.
    pub fn commit(&mut self, slot: u64, entries: &[Entry]) -> Result<bool, String> {
        if let Some(agreed) = self.log.get(&slot) {
            return if agreed == entries {
                Ok(false)
            } else {
                Err(format!("agreement broken at slot {slot}: {agreed:?} vs {entries:?}"))
            };
        }
        for e in entries {
            if !self.proposed.contains(&e.payload) {
                return Err(format!("validity: {:?} was never proposed", e.payload));
            }
            if let Some(at) = self.committed_at.insert(e.payload.clone(), slot) {
                return Err(format!("{:?} committed twice, in slots {at} and {slot}", e.payload));
            }
        }
        self.log.insert(slot, entries.to_vec());
        Ok(true)
    }

    /// `origin` reported its proposal `id` with `payload` as committed in `slot`, which is
    /// exact unless an installed snapshot resolved it (then the proposal is at or before it).
    pub fn acknowledged(
        &self,
        origin: NodeId,
        id: ProposalId,
        payload: &Bytes,
        slot: u64,
    ) -> Result<(), String> {
        let exact = |e: &Entry| e.origin == origin && e.id == id && e.payload == *payload;
        if self.log.range(..=slot).any(|(_, es)| es.iter().any(exact)) {
            return Ok(());
        }
        let at: Vec<_> = self
            .log
            .iter()
            .filter(|(_, es)| es.iter().any(|e| e.payload == *payload))
            .map(|(s, _)| s)
            .collect();
        Err(format!(
            "{origin:?} reported Committed {id:?} at slot {slot} but it is not logged by then (payload {payload:?} logged at {at:?})"
        ))
    }

    /// Whether `payload` was committed in some slot.
    pub fn is_committed(&self, payload: &Bytes) -> bool {
        self.committed_at.contains_key(payload)
    }

    /// Slots decided so far.
    pub fn slots(&self) -> usize {
        self.log.len()
    }

    /// Entries committed so far.
    pub fn entries(&self) -> usize {
        self.committed_at.len()
    }

    /// Payloads proposed so far.
    pub fn proposals(&self) -> usize {
        self.proposed.len()
    }
}

fn show(p: &ProposalView) -> String {
    format!("prio {:#x} value {:?}", p.priority, p.value())
}

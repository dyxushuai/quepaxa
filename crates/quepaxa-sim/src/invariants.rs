//! The paper's global invariants checked across real replicas (spec §9; paper deep read
//! §10), on protocol-internal state exposed by `Replica::inspect` (feature `model`).
//!
//! - **G1 agreement**: every replica that knows a slot's decision knows the same value.
//! - **G3 value lock**: once some proposer decided `v` in round `r`, every value stored at a
//!   step `≥ 4(r+1)` (and every previous-step aggregate above it) carries `v`.
//! - **G4 fast-path lock**: if a majority of recorders hold the leader's top-priority
//!   proposal as the first value of step 4, the slot can only decide that value.
//! - **G5 unique H**: at most one distinct value carries the leader priority per slot.

use std::collections::{BTreeMap, BTreeSet};

use quepaxa::inspect::{Inspection, ProposalView};

type Value = (u32, u64, u64);

/// Checks G1, G3, G4 and G5 over the live replicas of a cluster of `n`.
pub fn check(n: usize, replicas: &[(usize, Inspection)]) -> Result<(), String> {
    let mut decided: BTreeMap<u64, BTreeSet<Value>> = BTreeMap::new();
    let mut lock_step: BTreeMap<u64, u32> = BTreeMap::new();
    for (_, ins) in replicas {
        for (slot, v) in &ins.decided {
            decided.entry(*slot).or_default().insert(v.value());
        }
        for (slot, v, step) in &ins.decided_here {
            decided.entry(*slot).or_default().insert(v.value());
            let lock = 4 * (step / 4 + 1);
            lock_step.entry(*slot).and_modify(|l| *l = (*l).min(lock)).or_insert(lock);
        }
    }
    for (slot, values) in &decided {
        if values.len() > 1 {
            return Err(format!("G1 agreement: slot {slot} decided {values:?}"));
        }
    }
    let mut leader_values: BTreeMap<u64, BTreeSet<Value>> = BTreeMap::new();
    let mut fast_locks: BTreeMap<u64, BTreeMap<Value, usize>> = BTreeMap::new();
    for (i, ins) in replicas {
        for (slot, reg) in &ins.registers {
            let fields = [reg.first, reg.agg, reg.prev_agg];
            for p in fields.iter().flatten().filter(|p| p.priority == u64::MAX) {
                leader_values.entry(*slot).or_default().insert(p.value());
            }
            if reg.step == 4
                && let Some(f) = reg.first.filter(|f| f.priority == u64::MAX)
            {
                *fast_locks.entry(*slot).or_default().entry(f.value()).or_default() += 1;
            }
            if let (Some(lock), Some(v)) = (lock_step.get(slot), decided.get(slot).and_then(|s| s.first())) {
                let current = [reg.first, reg.agg].into_iter().filter(|_| reg.step >= *lock);
                let previous = [reg.prev_agg].into_iter().filter(|_| reg.step > *lock);
                for p in current.chain(previous).flatten() {
                    if p.value() != *v {
                        return Err(format!(
                            "G3 value lock: n{i} slot {slot} step {} holds {:?}, decided {v:?} locks from step {lock}",
                            reg.step,
                            show(&p)
                        ));
                    }
                }
            }
        }
    }
    for (slot, values) in &leader_values {
        if values.len() > 1 {
            return Err(format!("G5 unique H: slot {slot} has leader-priority values {values:?}"));
        }
    }
    for (slot, counts) in &fast_locks {
        for (v, count) in counts {
            if *count > n / 2
                && let Some(d) = decided.get(slot).and_then(|s| s.first())
                && d != v
            {
                return Err(format!(
                    "G4 fast-path lock: slot {slot} locked to {v:?} by a majority but decided {d:?}"
                ));
            }
        }
    }
    Ok(())
}

fn show(p: &ProposalView) -> String {
    format!("prio {:#x} value {:?}", p.priority, p.value())
}

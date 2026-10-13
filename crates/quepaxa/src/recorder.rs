//! Interval summary register: the recorder's per-slot state (paper Alg. 3).

use crate::proposal::Proposal;
use crate::types::Step;

/// Recorder state for one slot. `None` is the paper's nil.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Register {
    /// Current step `S`.
    pub step: Step,
    /// First value recorded in the current step (`Fc`).
    pub first: Option<Proposal>,
    /// Maximum value recorded in the current step (`Ac`).
    pub agg: Option<Proposal>,
    /// Maximum value recorded in the previous step (`Ap`).
    pub prev_agg: Option<Proposal>,
}

/// What a recorder reports back: `(S, Fc, Ap)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Summary {
    pub step: Step,
    pub first: Option<Proposal>,
    pub prev_agg: Option<Proposal>,
}

impl Register {
    /// Applies `record(s, v)`; returns whether the register changed (and so must be persisted).
    pub fn record(&mut self, s: Step, v: Proposal) -> bool {
        if s == self.step {
            if self.agg.as_ref().is_some_and(|a| *a >= v) {
                return false;
            }
            self.agg = Some(v);
            true
        } else if s > self.step {
            self.prev_agg = if s == self.step + 1 { self.agg.take() } else { None };
            self.step = s;
            self.first = Some(v.clone());
            self.agg = Some(v);
            true
        } else {
            false
        }
    }

    pub fn summary(&self) -> Summary {
        Summary { step: self.step, first: self.first.clone(), prev_agg: self.prev_agg.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proposal::tests::prop;
    use proptest::prelude::*;

    fn summary(step: Step, first: Option<Proposal>, prev_agg: Option<Proposal>) -> Summary {
        Summary { step, first, prev_agg }
    }

    #[test]
    fn first_record_sets_step_and_first_value_with_nil_prior() {
        let mut r = Register::default();
        assert!(r.record(4, prop(3, 1, 0)));
        assert_eq!(r.summary(), summary(4, Some(prop(3, 1, 0)), None));
    }

    #[test]
    fn same_step_higher_value_raises_aggregate_but_keeps_first() {
        let mut r = Register::default();
        r.record(4, prop(3, 1, 0));
        assert!(r.record(4, prop(7, 2, 0)));
        assert_eq!(r.agg, Some(prop(7, 2, 0)));
        assert_eq!(r.first, Some(prop(3, 1, 0)));
    }

    #[test]
    fn same_step_lower_or_equal_value_changes_nothing() {
        let mut r = Register::default();
        r.record(4, prop(7, 2, 0));
        let before = r.clone();
        assert!(!r.record(4, prop(3, 1, 0)));
        assert!(!r.record(4, prop(7, 2, 0)));
        assert_eq!(r, before);
    }

    #[test]
    fn next_step_moves_aggregate_to_prior() {
        let mut r = Register::default();
        r.record(4, prop(3, 1, 0));
        r.record(4, prop(7, 2, 0));
        r.record(5, prop(1, 3, 0));
        assert_eq!(r.summary(), summary(5, Some(prop(1, 3, 0)), Some(prop(7, 2, 0))));
    }

    #[test]
    fn skipping_a_step_clears_prior_aggregate() {
        let mut r = Register::default();
        r.record(4, prop(7, 2, 0));
        r.record(6, prop(1, 3, 0));
        assert_eq!(r.summary(), summary(6, Some(prop(1, 3, 0)), None));
    }

    #[test]
    fn stale_step_is_discarded_and_reports_current_state() {
        let mut r = Register::default();
        r.record(4, prop(7, 2, 0));
        r.record(5, prop(1, 3, 0));
        let before = r.clone();
        assert!(!r.record(4, prop(9, 9, 0)));
        assert_eq!(r, before);
        assert_eq!(r.summary(), summary(5, Some(prop(1, 3, 0)), Some(prop(7, 2, 0))));
    }

    fn arb_input() -> impl Strategy<Value = (Step, Proposal)> {
        (4u32..12, 0u64..6, 0u32..3).prop_map(|(s, p, o)| (s, prop(p, o, 0)))
    }

    proptest! {
        // Paper invariants R1–R6 (map ticket "QuePaxa protocol, precisely", §10).
        #[test]
        fn register_upholds_recorder_invariants(inputs in proptest::collection::vec(arb_input(), 1..40)) {
            let mut r = Register::default();
            for (s, v) in inputs {
                let before = r.clone();
                let changed = r.record(s, v.clone());
                prop_assert!(r.step >= before.step, "R1 step monotonic");
                prop_assert_eq!(changed, r != before);
                if s < before.step {
                    prop_assert_eq!(&r, &before, "R5 stale discard");
                } else if s == before.step {
                    prop_assert_eq!(&r.first, &before.first, "R2 first write-once");
                    prop_assert_eq!(&r.agg, &core::cmp::max(before.agg.clone(), Some(v)), "R3");
                } else {
                    let expect = if s == before.step + 1 { before.agg.clone() } else { None };
                    prop_assert_eq!(&r.prev_agg, &expect, "R4 prior aggregate");
                    prop_assert_eq!(&r.first, &Some(v), "first value of new step");
                }
                prop_assert_eq!(r.summary(), Summary { step: r.step, first: r.first.clone(), prev_agg: r.prev_agg.clone() }, "R6");
            }
        }
    }
}

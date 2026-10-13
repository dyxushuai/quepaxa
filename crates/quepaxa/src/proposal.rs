use bytes::Bytes;
use core::cmp::Ordering;

use crate::types::NodeId;

/// Priority reserved for the leader's round-1 proposal.
pub(crate) const LEADER_PRIORITY: u64 = u64::MAX;

/// Identity of one distinct proposed value: unique per (origin, incarnation, batch).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ValueKey {
    pub origin: NodeId,
    pub incarnation: u64,
    pub seq: u64,
}

/// A value paired with a priority, ordered by `(priority, key)`.
///
/// Equality and ordering ignore `value`: a key identifies exactly one value, so two
/// proposals with the same key carry the same bytes.
#[derive(Clone, Debug)]
pub(crate) struct Proposal {
    pub priority: u64,
    pub key: ValueKey,
    pub value: Bytes,
}

impl PartialEq for Proposal {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Proposal {}

impl core::hash::Hash for Proposal {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        (self.priority, self.key).hash(state);
    }
}

impl PartialOrd for Proposal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Proposal {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.priority, self.key).cmp(&(other.priority, other.key))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn prop(priority: u64, origin: u32, seq: u64) -> Proposal {
        Proposal {
            priority,
            key: ValueKey { origin: NodeId(origin), incarnation: 1, seq },
            value: Bytes::from(alloc::format!("v{origin}.{seq}")),
        }
    }

    #[test]
    fn higher_priority_wins_regardless_of_key() {
        assert!(prop(2, 0, 0) > prop(1, 9, 9));
    }

    #[test]
    fn equal_priority_breaks_ties_by_key() {
        assert!(prop(5, 2, 0) > prop(5, 1, 7));
    }

    #[test]
    fn leader_priority_beats_every_random_priority() {
        assert!(prop(LEADER_PRIORITY, 0, 0) > prop(LEADER_PRIORITY - 1, 9, 9));
    }

    #[test]
    fn nil_is_below_every_proposal() {
        assert!(None < Some(prop(0, 0, 0)));
    }

    fn arb_prop() -> impl proptest::strategy::Strategy<Value = Proposal> {
        use proptest::strategy::Strategy;
        (0u64..4, 0u32..3, 0u64..3).prop_map(|(p, o, q)| prop(p, o, q))
    }

    proptest::proptest! {
        #[test]
        fn distinct_keys_never_compare_equal(a in arb_prop(), b in arb_prop()) {
            proptest::prop_assert_eq!(a == b, a.priority == b.priority && a.key == b.key);
        }

        #[test]
        fn order_is_total_and_transitive(a in arb_prop(), b in arb_prop(), c in arb_prop()) {
            proptest::prop_assert_eq!(a.cmp(&b), b.cmp(&a).reverse());
            if a <= b && b <= c {
                proptest::prop_assert!(a <= c);
            }
        }
    }
}

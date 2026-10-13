/// Identity of one replica in the cluster.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u32);

/// Position in the replicated log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot(pub u64);

/// Logical clock of one slot: `4 * round + phase`, starting at 4 (round 1, phase 0).
pub(crate) type Step = u32;

/// Monotonic time in nanoseconds, supplied by the caller; the epoch is the caller's choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(pub u64);

impl Timestamp {
    /// `self + nanos`, saturating.
    pub fn after(self, nanos: u64) -> Self {
        Self(self.0.saturating_add(nanos))
    }
}

/// Identifier of a local proposal, unique within one incarnation of a replica.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProposalId(pub u64);

/// Identifier of a [`crate::Action::Persist`]; strictly increasing within one incarnation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PersistId(pub u64);

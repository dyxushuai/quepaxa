/// Identity of one replica in the cluster. Ids are chosen by the operator and must be
/// unique; they never change for a replica's lifetime (its durable state is tied to it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u32);

/// Position in the replicated log, from 0. Each slot holds one decided batch of entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot(pub u64);

/// Logical clock of one slot: `4 * round + phase`, starting at 4 (round 1, phase 0).
pub(crate) type Step = u32;

/// Steps beyond this are rejected as malformed, which keeps step arithmetic from
/// overflowing. Correct replicas stay far below it (a slot needs more than ~256 rounds only
/// with negligible probability, paper §4.2.2 fn. 5); only a Byzantine member, outside the
/// crash-fault model, could push a slot near it.
pub(crate) const MAX_STEP: Step = u32::MAX - 16;

/// Slots past this are rejected so slot arithmetic (window, reservations) cannot overflow.
pub(crate) const MAX_SLOT: u64 = u64::MAX - (1 << 32);

/// Monotonic time in nanoseconds, supplied by the caller with every call; the epoch is the
/// caller's choice (process start is typical). Only differences matter, and the replica
/// never lets its clock move backwards, so a slightly stale reading is harmless.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(pub u64);

impl Timestamp {
    /// `self + nanos`, saturating.
    pub fn after(self, nanos: u64) -> Self {
        Self(self.0.saturating_add(nanos))
    }
}

/// Identifier of a local proposal, returned by [`crate::Replica::propose`] (and
/// `read_barrier`, `propose_reconfiguration`), unique within one incarnation of a
/// replica. Entries in [`crate::Action::Commit`] carry it with their origin and
/// incarnation, which together identify an entry cluster-wide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProposalId(pub u64);

/// Identifier of a [`crate::Action::Persist`]; strictly increasing within one incarnation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PersistId(pub u64);

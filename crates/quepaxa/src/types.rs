/// Identity of one replica in the cluster.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u32);

/// Position in the replicated log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot(pub u64);

/// Logical clock of one slot: `4 * round + phase`, starting at 4 (round 1, phase 0).
pub(crate) type Step = u32;

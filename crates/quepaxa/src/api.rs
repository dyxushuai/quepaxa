//! Public configuration, inputs, actions and errors of a [`crate::Replica`].

use alloc::vec::Vec;
use bytes::Bytes;
use core::fmt;

use crate::batch::Entry;
use crate::types::{NodeId, PersistId, ProposalId, Slot};

const MILLI: u64 = 1_000_000;

/// Envelope and proposal framing budget on top of the values a message carries.
const MESSAGE_OVERHEAD: u64 = 1024;

/// Static configuration of one replica. Every member must use the same `cluster_id` and
/// `members`; `seed` must be fresh CSPRNG output on every start, including restarts.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "model", derive(Hash))]
#[non_exhaustive]
pub struct Config {
    /// Identifies the cluster; messages from other clusters are dropped.
    pub cluster_id: [u8; 16],
    /// This replica.
    pub id: NodeId,
    /// All replicas, including this one.
    pub members: Vec<NodeId>,
    /// Randomness for proposal priorities. Liveness relies on the network being unable to
    /// predict it, so never reuse a seed across starts.
    pub seed: [u8; 32],
    /// Upper bound on the encoded entries of one batch (one slot).
    pub max_batch_bytes: u32,
    /// Upper bound on one encoded network message; larger inputs are dropped unread. Must be
    /// at least `2 * max_batch_bytes + 1024` (a reply can carry two full values).
    pub max_message_bytes: u32,
    /// How many slots this replica may drive concurrently.
    pub max_in_flight_slots: u32,
    /// How many local proposals may be pending before [`Error::Busy`].
    pub max_outstanding_proposals: u32,
    /// How often an unanswered record request is re-sent, in nanoseconds.
    pub retransmit_interval: u64,
    /// Lower bound of the adaptive hedge delay, in nanoseconds.
    pub hedge_min: u64,
    /// Upper bound of the adaptive hedge delay, in nanoseconds; a quarter of it is used
    /// until a commit latency has been observed.
    pub hedge_max: u64,
}

impl Config {
    /// A configuration with default limits and timers.
    pub fn new(cluster_id: [u8; 16], id: NodeId, members: Vec<NodeId>, seed: [u8; 32]) -> Self {
        Self {
            cluster_id,
            id,
            members,
            seed,
            max_batch_bytes: 1 << 20,
            max_message_bytes: 16 << 20,
            max_in_flight_slots: 8,
            max_outstanding_proposals: 4096,
            retransmit_interval: 50 * MILLI,
            hedge_min: MILLI,
            hedge_max: 1000 * MILLI,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), Error> {
        let mut sorted = self.members.clone();
        sorted.sort_unstable();
        sorted.dedup();
        let checks = [
            (sorted.len() == self.members.len(), "members must be unique"),
            (sorted.contains(&self.id), "id must be a member"),
            (self.max_batch_bytes > 0, "max_batch_bytes must be positive"),
            (
                u64::from(self.max_message_bytes) >= 2 * u64::from(self.max_batch_bytes) + MESSAGE_OVERHEAD,
                "max_message_bytes must hold two full batches plus 1 KiB of framing",
            ),
            (self.max_in_flight_slots > 0, "max_in_flight_slots must be positive"),
            (
                u64::from(self.max_in_flight_slots) <= crate::schedule::LAG,
                "max_in_flight_slots must be at most 64",
            ),
            (
                self.hedge_max > 0 && self.hedge_min <= self.hedge_max,
                "hedge bounds must satisfy 0 <= hedge_min <= hedge_max, hedge_max > 0",
            ),
            (self.max_outstanding_proposals > 0, "max_outstanding_proposals must be positive"),
            (self.retransmit_interval > 0, "retransmit_interval must be positive"),
        ];
        match checks.iter().find(|(ok, _)| !ok) {
            Some((_, why)) => Err(Error::InvalidConfig(why)),
            None => Ok(()),
        }
    }
}

/// Something that happened outside the replica.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Input {
    /// A network message from a peer, exactly as received.
    Message(Bytes),
    /// Every [`Action::Persist`] with an id up to and including `id` is durable.
    Persisted(PersistId),
    /// Persisting `id` failed; the replica fences itself until restarted.
    StorageFailed(PersistId),
}

/// A message to send to one peer.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub struct Outbound {
    /// Destination replica.
    pub to: NodeId,
    /// Encoded message.
    pub bytes: Bytes,
}

/// Why a local proposal will never commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
#[non_exhaustive]
pub enum DropReason {
    /// The replica fenced itself after a storage failure.
    Fenced,
}

/// Work the caller must carry out, in the order returned.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
#[non_exhaustive]
pub enum Action {
    /// Durably append `records` after every earlier `Persist`, then send `then`, then
    /// report [`Input::Persisted`]. Never send `then` before `records` are durable.
    Persist {
        /// Completion token to report back.
        id: PersistId,
        /// Opaque records; hand all of them back, in order, to [`crate::Replica::new`] on restart.
        records: Vec<Bytes>,
        /// Messages gated on this write.
        then: Vec<Outbound>,
        /// Replace every previously persisted record with `records` (atomically, e.g.
        /// write-new-then-rename) instead of appending. Emitted after compaction.
        replace: bool,
    },
    /// Send now; no durability required.
    Send(Outbound),
    /// Apply this slot's entries; slots arrive in strict order without gaps. After a
    /// restart, slots recovered from storage are emitted again.
    Commit {
        /// The slot.
        slot: Slot,
        /// Entries decided in it, possibly none.
        entries: Vec<Entry>,
    },
    /// A local proposal was committed.
    Committed {
        /// The proposal.
        id: ProposalId,
        /// Where it was committed.
        slot: Slot,
    },
    /// A read barrier is in the log: after applying every commit up to and including
    /// `slot`, application state may serve a linearizable read.
    ReadReady {
        /// The barrier.
        id: ProposalId,
        /// The barrier's slot.
        slot: Slot,
    },
    /// Slots up to `through` were compacted away at `peer`: fetch that peer's
    /// application snapshot and pass it to [`crate::Replica::install_snapshot`].
    SnapshotNeeded {
        /// A replica that has the snapshot.
        peer: NodeId,
        /// The last slot it covers (at least).
        through: Slot,
    },
    /// A local proposal will never commit.
    Dropped {
        /// The proposal.
        id: ProposalId,
        /// Why.
        reason: DropReason,
    },
}

/// Counters for observability.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
#[non_exhaustive]
pub struct Metrics {
    /// Network messages that failed to decode or came from a non-member.
    pub messages_rejected: u64,
    /// Record requests re-sent after `retransmit_interval`.
    pub retransmits: u64,
    /// Slots this replica learned as decided.
    pub slots_decided: u64,
    /// Decisions this replica's proposers reached on the leader fast path.
    pub fast_path_decisions: u64,
    /// Decided batches whose entries could not be decoded.
    pub corrupt_batches: u64,
    /// Batches of entries forwarded to the leader.
    pub forwarded: u64,
    /// Own entries proposed directly after the leader did not commit them in time.
    pub hedged: u64,
    /// Gauge: decided slots held in memory (bounded by compaction).
    pub retained_slots: u64,
    /// Gauge: recorder registers held in memory.
    pub live_registers: u64,
}

/// A caller contract violation or a refused request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The configuration is invalid.
    InvalidConfig(&'static str),
    /// A durable record handed to [`crate::Replica::new`] is corrupt; `index` is its position.
    CorruptRecord {
        /// Position in the records passed in.
        index: usize,
    },
    /// Too many outstanding proposals; retry after some commit.
    Busy,
    /// The payload does not fit in one batch.
    PayloadTooLarge,
    /// The replica is fenced after a storage failure.
    Fenced,
    /// Actions from the previous call were not drained with `poll_actions`.
    NotDrained,
    /// `Persisted` named an id that was never issued or was already reported.
    UnknownPersistId,
    /// The core state passed to `compact` or `install_snapshot` is malformed or does not
    /// match the slot it claims to cover.
    InvalidCoreState,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidConfig(why) => write!(f, "invalid config: {why}"),
            Error::CorruptRecord { index } => write!(f, "durable record {index} is corrupt"),
            Error::Busy => f.write_str("too many outstanding proposals"),
            Error::PayloadTooLarge => f.write_str("payload exceeds max_batch_bytes"),
            Error::Fenced => f.write_str("replica is fenced after a storage failure"),
            Error::NotDrained => f.write_str("poll_actions must drain actions before the next call"),
            Error::UnknownPersistId => f.write_str("unknown persist id"),
            Error::InvalidCoreState => f.write_str("invalid core state"),
        }
    }
}

impl core::error::Error for Error {}

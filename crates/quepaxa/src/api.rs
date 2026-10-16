//! Public configuration, inputs, actions and errors of a [`crate::Replica`].

use alloc::vec::Vec;
use bytes::Bytes;
use core::fmt;

use crate::batch::Entry;
use crate::types::{NodeId, PersistId, ProposalId, Slot};

const MILLI: u64 = 1_000_000;

/// Envelope and proposal framing budget on top of the values a message carries.
const MESSAGE_OVERHEAD: u64 = 1024;

/// Configuration of one replica, checked by [`crate::Replica::new`].
///
/// Every member must use the same `cluster_id`, `members` (the initial membership),
/// `leader_order` and `max_batch_bytes`; `seed` must be fresh CSPRNG output on every start,
/// including restarts. [`Config::new`] fills the limits and timers with defaults for a LAN;
/// [`guide::tuning`](crate::guide::tuning) explains each field and when to change it.
///
/// # Examples
///
/// ```
/// use quepaxa::{Config, NodeId};
///
/// const MILLI: u64 = 1_000_000;
/// let members = vec![NodeId(0), NodeId(1), NodeId(2)];
/// let mut cfg = Config::new(*b"orders-cluster-1", NodeId(1), members, [3; 32]);
/// // A cluster spread over regions with ~150 ms round trips:
/// cfg.retransmit_interval = 400 * MILLI;
/// cfg.hedge_max = 3_000 * MILLI;
/// cfg.leader_order = vec![NodeId(1), NodeId(0), NodeId(2)];
/// ```
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
    /// Upper bound on the encoded entries of one batch (one slot), at least 64. Cluster-wide:
    /// every member must use the same value.
    pub max_batch_bytes: u32,
    /// Upper bound on one encoded network message; larger inputs are dropped unread. Must be
    /// at least `2 * max_batch_bytes + 1024` (a reply can carry two full values).
    pub max_message_bytes: u32,
    /// How many slots this replica may drive concurrently.
    pub max_in_flight_slots: u32,
    /// How many local proposals may be pending before [`Error::Busy`].
    pub max_outstanding_proposals: u32,
    /// How often an unanswered record request is re-sent, in nanoseconds. The default
    /// (50 ms) suits a LAN; across regions set it above the round trip to the farthest
    /// member of a majority, or every request is sent several times.
    pub retransmit_interval: u64,
    /// Lower bound of the adaptive hedge delay, in nanoseconds.
    pub hedge_min: u64,
    /// Upper bound of the adaptive hedge delay, in nanoseconds; a quarter of it is used
    /// until a commit latency has been observed. Keep it above the slowest client commit
    /// path (through the leader) or followers hedge routinely, which costs throughput.
    pub hedge_max: u64,
    /// Preferred leaders, first to last: every member once. Empty (the default) means by
    /// id. Put the member with the fastest path to a majority first: on a WAN the
    /// placement alone can change client latency about 2x. The leader moves down this
    /// order only when it stops getting its values decided.
    ///
    /// Members with different orders would disagree on who leads, so the order is folded
    /// into the cluster identity on the wire: such members ignore each other. It is fixed
    /// for the life of the cluster's state: a replica refuses to start from, or install, a
    /// snapshot built under another order.
    pub leader_order: Vec<NodeId>,
}

impl Config {
    /// A configuration with default limits and timers (see the table in
    /// [`guide::tuning`](crate::guide::tuning)): 1 MiB batches, 16 MiB messages, 8 slots in
    /// flight, 4096 outstanding proposals, 50 ms retransmission, hedging between 1 ms and
    /// 1 s, leaders by id.
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
            leader_order: Vec::new(),
        }
    }

    /// The leader order in effect: `leader_order`, or the members by id.
    pub(crate) fn leaders(&self) -> Vec<NodeId> {
        if self.leader_order.is_empty() {
            let mut order = self.members.clone();
            order.sort_unstable();
            order
        } else {
            self.leader_order.clone()
        }
    }

    /// `cluster_id` as sent on the wire: a custom leader order is mixed in, so members
    /// configured with different orders never exchange messages.
    pub(crate) fn wire_cluster_id(&self) -> [u8; 16] {
        let mut id = self.cluster_id;
        let mut by_id = self.members.clone();
        by_id.sort_unstable();
        if self.leaders() != by_id {
            let fnv = self
                .leader_order
                .iter()
                .flat_map(|n| n.0.to_le_bytes())
                .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
            for (b, f) in id[8..].iter_mut().zip(fnv.to_le_bytes()) {
                *b ^= f;
            }
        }
        id
    }

    pub(crate) fn validate(&self) -> Result<(), Error> {
        let mut sorted = self.members.clone();
        sorted.sort_unstable();
        sorted.dedup();
        let checks = [
            (!self.members.is_empty(), "members must not be empty"),
            (self.members.len() <= crate::schedule::MAX_MEMBERS, "at most 1024 members"),
            (sorted.len() == self.members.len(), "members must be unique"),
            // Room for at least one entry's framing (an empty read barrier is 29 bytes).
            (self.max_batch_bytes >= 64, "max_batch_bytes must be at least 64"),
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
            (
                self.leader_order.is_empty() || {
                    let mut order = self.leader_order.clone();
                    order.sort_unstable();
                    order == sorted
                },
                "leader_order must list every member exactly once",
            ),
        ];
        match checks.iter().find(|(ok, _)| !ok) {
            Some((_, why)) => Err(Error::InvalidConfig(why)),
            None => Ok(()),
        }
    }
}

/// Something that happened outside the replica, fed with [`crate::Replica::handle`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Input {
    /// A network message from a peer, exactly as received.
    Message(Bytes),
    /// Every [`Action::Persist`] with an id up to and including `id` is durable.
    Persisted(PersistId),
    /// Persisting `id` failed. The replica fences itself: it reports outstanding proposals
    /// as [`Action::Dropped`], ignores further events and refuses requests with
    /// [`Error::Fenced`]. Restart it from its durable records.
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

/// Why a local proposal is no longer tracked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
#[non_exhaustive]
pub enum DropReason {
    /// The replica fenced itself after a storage failure. As after a crash, the outcome is
    /// unknown: the proposal may already have reached a quorum and may still commit.
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
    /// Apply this slot's entries; slots arrive in strict order without gaps, as soon as
    /// they are decided (the local record of the decision may still be in flight). After a
    /// restart, slots already returned are emitted again: recovered from storage, or
    /// learned again from peers.
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
        /// Where it was committed; if an installed snapshot resolved it, that snapshot's
        /// `through` (the proposal is in the log at or before it).
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
    /// A membership change was applied (in the log order of `Commit`): from slot `from`,
    /// `members` (preferred leaders first) are the replicas and majorities are theirs.
    /// A replica not in `members` may shut down once it has applied the slot before `from`.
    Reconfigured {
        /// The new epoch (the configured members are epoch 0).
        epoch: u64,
        /// First slot of the epoch.
        from: Slot,
        /// The members, preferred leaders first.
        members: Vec<NodeId>,
    },
    /// A local proposal will not be reported again (see [`DropReason`] for whether it may
    /// still commit).
    Dropped {
        /// The proposal.
        id: ProposalId,
        /// Why.
        reason: DropReason,
    },
}

/// Counters for observability, from [`crate::Replica::metrics`]. Counters only grow
/// within one incarnation; gauges (marked) go up and down.
/// [`guide::tuning`](crate::guide::tuning) says what unusual values indicate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
#[non_exhaustive]
pub struct Metrics {
    /// Network messages that failed to decode or came from a non-member.
    pub messages_rejected: u64,
    /// Record requests re-sent after `retransmit_interval`.
    pub retransmits: u64,
    /// Bulk catch-up requests re-sent after `retransmit_interval`.
    pub catch_up_retries: u64,
    /// Slots this replica learned as decided.
    pub slots_decided: u64,
    /// Decisions this replica's proposers reached on the leader fast path.
    pub fast_path_decisions: u64,
    /// Decided batches whose entries could not be decoded (committed as empty). Always 0
    /// unless replicas run incompatible versions.
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

/// Why a call was refused. A refused call changes nothing in the replica.
///
/// | Kind | Variants | What to do |
/// |---|---|---|
/// | Backpressure | [`Busy`](Error::Busy), [`ReconfigurationPending`](Error::ReconfigurationPending) | Retry later. |
/// | Bad input | [`InvalidConfig`](Error::InvalidConfig), [`PayloadTooLarge`](Error::PayloadTooLarge), [`InvalidCoreState`](Error::InvalidCoreState) | Fix the argument. |
/// | Storage | [`CorruptRecord`](Error::CorruptRecord), [`Fenced`](Error::Fenced) | Repair or restore the data; restart. |
/// | Driver bug | [`NotDrained`](Error::NotDrained), [`UnknownPersistId`](Error::UnknownPersistId) | Fix the event loop ([`guide::driving`](crate::guide::driving)). |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The configuration (or a proposed membership) breaks a documented rule; the message
    /// names it.
    InvalidConfig(&'static str),
    /// A durable record handed to [`crate::Replica::new`] is corrupt; `index` is its position.
    CorruptRecord {
        /// Position in the records passed in.
        index: usize,
    },
    /// [`Config::max_outstanding_proposals`] local proposals are already waiting; retry after
    /// some commit.
    Busy,
    /// The payload alone does not fit in one batch ([`Config::max_batch_bytes`]).
    PayloadTooLarge,
    /// The replica fenced itself after [`Input::StorageFailed`]; restart it.
    Fenced,
    /// Actions from the previous call were not taken with
    /// [`poll_actions`](crate::Replica::poll_actions); this call did nothing.
    NotDrained,
    /// `Persisted` named an id that was never issued or was already reported.
    UnknownPersistId,
    /// The core state passed to `compact` or `install_snapshot` is malformed or does not
    /// match the slot it claims to cover.
    InvalidCoreState,
    /// A membership change is still waiting to take effect; propose the next one after its
    /// [`Action::Reconfigured`] slot is committed.
    ReconfigurationPending,
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
            Error::ReconfigurationPending => f.write_str("a membership change has not taken effect yet"),
        }
    }
}

impl core::error::Error for Error {}

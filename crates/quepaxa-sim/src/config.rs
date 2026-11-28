//! The knobs of a run and what it reports.

/// Write rates per replica, in steps: each applies from its virtual ms until the next.
pub type WriteWeights = Vec<(u64, Vec<u32>)>;

/// Fault and workload knobs. Rates are per mille.
#[derive(Clone, Debug)]
pub struct SimConfig {
    /// Cluster size.
    pub nodes: usize,
    /// Master seed; the same seed replays the same run.
    pub seed: u64,
    /// Virtual duration of the fault phase.
    pub fault_phase_ms: u64,
    /// Virtual time allowed to converge after faults stop.
    pub heal_budget_ms: u64,
    /// Probability that a sent message is lost.
    pub drop: u32,
    /// Probability that a sent message is duplicated.
    pub dup: u32,
    /// Probability per tick that a minority is partitioned off.
    pub partition: u32,
    /// Probability per tick that some replica crashes.
    pub crash: u32,
    /// Probability per tick that some replica receives a client write.
    pub propose: u32,
    /// Probability per tick that some replica receives a client read.
    pub read: u32,
    /// Upper bound on one-way message latency (jitter on top of `links_ms`).
    pub max_latency_ms: u64,
    /// Base one-way latency from replica `i` to replica `j` (a WAN placement); none on loopback.
    pub links_ms: Option<Vec<Vec<u64>>>,
    /// Replica `retransmit_interval`, in milliseconds.
    pub retransmit_ms: u64,
    /// Replica `hedge_max`, in milliseconds.
    pub hedge_max_ms: u64,
    /// Replica `silent_leader_takeover`.
    pub silent_leader_takeover: bool,
    /// Extra replicas that start outside the cluster and may join it by reconfiguration.
    pub spares: usize,
    /// Permanent non-voting observer replicas: started outside the cluster with `follow_interval`
    /// set, never added by reconfiguration, must converge to the full log and serve linearizable
    /// reads without ever retiring (#157).
    pub observers: usize,
    /// Probability per tick (per mille) of proposing a new membership drawn from members
    /// and spares, of any size from one replica to all of them, or a new leader order.
    pub reconfigure: u32,
    /// At `.1` virtual ms, replica `.0` proposes the same members with leader order `.2`.
    pub reorder: Option<(usize, u64, Vec<usize>)>,
    /// Upper bound on one persist's latency.
    pub max_persist_ms: u64,
    /// Replica limit on concurrent slots.
    pub in_flight_slots: u32,
    /// A replica kept down for the whole fault phase (it starts at heal).
    pub absent: Option<usize>,
    /// Crash this replica at this virtual time and keep it down until heal.
    pub kill: Option<(usize, u64)>,
    /// Stall these replicas' disks from `.1` for `.2` virtual ms (a shared disk's writeback
    /// burst): writes started meanwhile finish only after it.
    pub disk_stall: Option<(Vec<usize>, u64, u64)>,
    /// Slow these replicas' disks from `.1` for `.2` virtual ms: writes started meanwhile take
    /// up to `slow_persist_ms` instead of `max_persist_ms`.
    pub disk_slow: Option<(Vec<usize>, u64, u64)>,
    /// Cut this replica off from the others from `.1` for `.2` virtual ms, in place of random
    /// partitions.
    pub isolate: Option<(usize, u64, u64)>,
    /// Upper bound on one persist's latency on a slowed disk.
    pub slow_persist_ms: u64,
    /// These replicas run on a slow machine: they handle one message at a time, each taking
    /// this many virtual microseconds, and later messages wait their turn.
    pub slow_cpu: Option<(Vec<usize>, u64)>,
    /// Snapshot and compact every this many applied slots.
    pub compact_every: Option<u64>,
    /// Replica message size limit (small values force multi-page catch-up).
    pub max_message_bytes: u32,
    /// Leader placement on every replica, with its sustained period and hold (virtual ms).
    pub placement: Option<(u64, u64)>,
    /// Relative write rates per replica (by index; others write nothing), changing at the
    /// given virtual times. None: every member writes equally.
    pub write_weights: Option<WriteWeights>,
    /// Record in the report when the leader in force changes (costs a lookup per tick).
    pub track_leader: bool,
    /// Target write payload size range `(min, max)` in bytes (`None` for small ~10-byte
    /// payloads). Values must fit within `max_batch_bytes` (4096 in the simulator).
    pub payload_bytes: Option<(usize, usize)>,
    /// Enable online `in_flight_slots` tuning (`Config::tune_in_flight_slots`).
    pub tune_in_flight_slots: bool,
    /// Optional bursty write-rate schedule `(at_ms, propose_per_mille)` overriding `propose`
    /// from each virtual ms onward. Values above 1000 issue multiple writes per tick.
    pub propose_schedule: Option<Vec<(u64, u32)>>,
}

/// Draws `Config::inline_batch_bytes` per replica per seed from `{64, 256, u32::MAX}` so
/// mixed clusters run in every simulation seed without perturbing the main RNG stream.
pub(crate) fn inline_batch_bytes(seed: u64, node: usize) -> u32 {
    let mut r = crate::SplitMix64::new(seed ^ (node as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9));
    [64, 256, u32::MAX][r.below(3) as usize]
}

impl SimConfig {
    /// How long a write proposed at a member of a connected majority may take to commit
    /// (R4), or `None` if links lose too much for a majority to count as connected.
    ///
    /// A forward the network loses is sent again when the leader has not acknowledged it
    /// within twice its acknowledgement delay (at least a retransmit interval; `hedge_max`
    /// before any acknowledgement), backing off up to `hedge_max`;
    /// an entry whose forwards are all lost is hedged 4δ (at most `hedge_max`) after the
    /// first, and the hedged slot is lost to the leader's (a few retransmit intervals, each
    /// lost record request doubling the wait). An acknowledged entry a busy leader keeps
    /// waiting is hedged at most 4 · `hedge_max` after its forward. Each loss costs at most
    /// one such round, and each round cuts the tail about tenfold or more. At 5% loss the slowest write took 1.8 s over 100,000 hostile
    /// seeds, and 2.6 s over 20,000 seeds without crashes and with rarer partitions (longer
    /// windows: 24 writes over 2 s, 3 over 2.4 s). Eight rounds (4 s there) leave three rounds
    /// of margin for the nightly seed count. At 25% loss, chains of losses outlast any bound
    /// shorter than the fault phase.
    pub fn progress_bound_ms(&self) -> Option<u64> {
        (self.drop <= 50).then_some(8 * (self.hedge_max_ms + 5 * self.retransmit_ms))
    }

    /// Long fsyncs, frequent crashes and heavy loss: maximizes the window in which a reply
    /// could escape before its write is durable.
    pub fn durability_stress(nodes: usize, seed: u64) -> Self {
        Self {
            drop: 250,
            crash: 25,
            max_persist_ms: 40,
            in_flight_slots: 2,
            compact_every: Some(2),
            ..Self::hostile(nodes, seed)
        }
    }

    /// A hostile default: lossy, reordering, partitioning, crashing.
    pub fn hostile(nodes: usize, seed: u64) -> Self {
        Self {
            nodes,
            seed,
            fault_phase_ms: 3_000,
            heal_budget_ms: 60_000,
            drop: 50,
            dup: 20,
            partition: 3,
            crash: 3,
            propose: 300,
            read: 150,
            max_latency_ms: 20,
            links_ms: None,
            retransmit_ms: 20,
            hedge_max_ms: 400,
            silent_leader_takeover: true,
            spares: 0,
            observers: 0,
            reconfigure: 0,
            reorder: None,
            max_persist_ms: 5,
            in_flight_slots: 4,
            absent: None,
            kill: None,
            disk_stall: None,
            disk_slow: None,
            isolate: None,
            slow_persist_ms: 40,
            slow_cpu: None,
            compact_every: Some(32),
            max_message_bytes: 1 << 20,
            placement: None,
            write_weights: None,
            track_leader: false,
            payload_bytes: None,
            tune_in_flight_slots: false,
            propose_schedule: None,
        }
    }
}

/// What a finished run observed.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Slots in the agreed log.
    pub slots: u64,
    /// Entries committed.
    pub entries: usize,
    /// Proposals made.
    pub proposals: usize,
    /// Linearizable reads completed and checked.
    pub reads: usize,
    /// Crashes injected.
    pub crashes: usize,
    /// Fingerprint of the event trace (same seed ⇒ same fingerprint).
    pub trace_hash: u64,
    /// The last lines of the trace, for debugging.
    pub trace_tail: String,
    /// Longest virtual time between two new agreed slots during the fault phase.
    pub max_commit_gap_ms: u64,
    /// Largest number of decided slots any replica held in memory.
    pub max_retained_slots: u64,
    /// Snapshots installed from peers.
    pub snapshots_installed: usize,
    /// New agreed slots during the fault phase after the configured kill.
    pub slots_after_kill: u64,
    /// Each live replica's counters at the end (since its last restart).
    pub metrics: Vec<quepaxa::Metrics>,
    /// Most uncommitted local batches any live replica held at the end.
    pub local_batches: usize,
    /// Propose-to-`Committed` latency of every acknowledged write, in microseconds.
    pub commit_latencies_us: Vec<u64>,
    /// Every acknowledged write: the replica it was proposed at, and when it was proposed
    /// and acknowledged (virtual microseconds).
    pub commits: Vec<(usize, u64, u64)>,
    /// Membership changes applied (by the first replica to apply each).
    pub reconfigurations: u64,
    /// The members of every epoch put in force, the configured ones first.
    pub epochs: Vec<Vec<usize>>,
    /// When any replica first reported the configured reorder decided (virtual microseconds).
    pub reorder_decided_us: Option<u64>,
    /// When the agreed log first reached the reorder's first slot: every slot before it is
    /// agreed and the new order governs the next (virtual microseconds).
    pub reorder_in_force_us: Option<u64>,
    /// Writes acknowledged while their replica was in a connected majority (R4).
    pub held_writes: u64,
    /// The leader order in force at the end, as the first live member sees it.
    pub final_order: Vec<usize>,
    /// With `SimConfig::track_leader`: from which virtual ms each leader led, as the first
    /// live member reported it.
    pub leader_timeline: Vec<(u64, usize)>,
    /// Suspected entry combinations (#187) every replica ran into, crashed and retired
    /// incarnations included.
    pub entry_probes: quepaxa::inspect::EntryProbes,
    /// Total message bytes each replica sent over the network.
    pub bytes_sent: Vec<u64>,
}

/// Bounds of one delay-bounded model-checking exploration.
#[derive(Clone, Debug)]
pub struct ModelConfig {
    /// Cluster size.
    pub nodes: usize,
    /// One client write per entry, proposed at that replica at the start.
    pub proposals: Vec<usize>,
    /// One linearizable read per entry, requested at that replica once the first write is
    /// acknowledged: it must cover that write.
    pub reads: Vec<usize>,
    /// Deviations from the default schedule along one path.
    pub deviations: u32,
    /// Crash-restarts allowed along one path (a separate budget; they cost no deviation).
    pub crashes: u32,
    /// Longest path explored.
    pub max_depth: usize,
    /// Distinct states explored before stopping.
    pub max_states: usize,
    /// One client write per entry, proposed at that replica once every write in
    /// `proposals` is acknowledged.
    pub later: Vec<usize>,
    /// A replica cut off once every write in `proposals` is acknowledged, as if it crashed
    /// then (one partition, costing no deviation; healed like any other). A follower judges
    /// silence only against a member it has handed entries to before.
    pub silenced: Option<usize>,
    /// Network delay of every message, in nanoseconds: a delivery moves the receiver's
    /// clock to at least the send time plus this. Zero keeps time still between timers;
    /// silence and hedge delays are measured from these delays.
    pub link_delay: u64,
    /// A replica whose persists the default schedule completes only once nothing else is
    /// pending, no timer included: a disk slower than every timeout.
    pub slow_disk: Option<usize>,
    /// A replica whose messages the default schedule delivers only once nothing else is
    /// pending, no timer included: a leader slower than every timeout.
    pub slow_net: Option<usize>,
    /// Replicas beyond `nodes`, started outside the membership (to be added by a change).
    pub spares: usize,
    /// A membership change proposed at the start: at that replica, the new members (of
    /// replicas `0..nodes + spares`), preferred leaders in that order (the same members in
    /// another order change only the leader).
    pub reconfigure: Option<(usize, Vec<usize>)>,
    /// A competing change proposed at the start at another replica, computed from the same
    /// configuration: whichever is decided second is ignored. Not proposed again after a
    /// crash.
    pub rival: Option<(usize, Vec<usize>)>,
    /// Replica `Config::inline_batch_bytes`; values below `u32::MAX` also pad client writes
    /// past it so forwarded batches travel by reference (`Data` + `Ref`).
    pub inline_batch_bytes: u32,
    /// Drops the origin's initial dissemination `Data` to this replica (before it has issued
    /// any `Fetch`), forcing it to fetch the batch.
    pub drop_data_to: Option<usize>,
    /// Permanently crashes this replica once every write in `proposals` is acknowledged.
    pub crash_after_warmup: Option<usize>,
    /// Permanently crashes this replica as soon as any recorder parks a request under rule R.
    pub crash_on_parked: Option<usize>,
    /// Each replica snapshots and compacts as soon as it commits a slot, serving
    /// `SnapshotNeeded` out of band.
    pub compact_on_commit: bool,
}

impl ModelConfig {
    /// A three-replica cluster with the given writes, no faults beyond the budgets.
    pub fn new(proposals: Vec<usize>, deviations: u32, crashes: u32) -> Self {
        Self {
            nodes: 3,
            proposals,
            reads: Vec::new(),
            deviations,
            crashes,
            max_depth: 200,
            max_states: 1_000_000,
            later: Vec::new(),
            silenced: None,
            link_delay: 0,
            slow_disk: None,
            slow_net: None,
            spares: 0,
            reconfigure: None,
            rival: None,
            inline_batch_bytes: u32::MAX,
            drop_data_to: None,
            crash_after_warmup: None,
            crash_on_parked: None,
            compact_on_commit: false,
        }
    }
}

/// What a model-checking exploration covered.
#[derive(Clone, Debug, Default)]
pub struct ModelReport {
    /// Distinct states visited.
    pub states: usize,
    /// States in which every replica committed every proposal.
    pub complete: usize,
    /// Whether every schedule within the bounds was explored.
    pub exhausted: bool,
    /// States at the depth bound; each was shown to settle under the default schedule.
    pub depth_limited: usize,
    /// States in which every client write had committed and every read finished; a
    /// replica cut off may still be behind.
    pub served: usize,
    /// States in which some replica had hedged an entry (proposed it itself after its
    /// target was late or suspected).
    pub hedged: usize,
    /// States in which some replica had stepped aside.
    pub stepped_aside: usize,
    /// States in which an entry had committed in a batch proposed by a deputy: neither the
    /// leader (n0) nor the entry's origin.
    pub deputy: usize,
    /// States in which some replica had applied a membership change.
    pub reconfigured: usize,
    /// Suspected entry combinations (#187) run into over every explored step.
    pub entry_probes: quepaxa::inspect::EntryProbes,
    /// States in which some replica had parked a record request missing content (rule R).
    pub parked: usize,
    /// States in which some replica had sent a `Fetch` for missing batch content.
    pub fetches: usize,
    /// States in which some replica had retried/rotated a `Fetch`.
    pub fetch_retries: usize,
    /// States in which some replica had installed a peer snapshot after `Compacted`.
    pub snapshots_installed: usize,
}

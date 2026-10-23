//! The knobs of a run and what it reports.

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
    /// Extra replicas that start outside the cluster and may join it by reconfiguration.
    pub spares: usize,
    /// Probability per tick (per mille) of proposing a new membership of `nodes` replicas
    /// drawn from members and spares.
    pub reconfigure: u32,
    /// Upper bound on one persist's latency.
    pub max_persist_ms: u64,
    /// Replica limit on concurrent slots.
    pub in_flight_slots: u32,
    /// A replica kept down for the whole fault phase (it starts at heal).
    pub absent: Option<usize>,
    /// Crash this replica at this virtual time and keep it down until heal.
    pub kill: Option<(usize, u64)>,
    /// Snapshot and compact every this many applied slots.
    pub compact_every: Option<u64>,
    /// Replica message size limit (small values force multi-page catch-up).
    pub max_message_bytes: u32,
}

impl SimConfig {
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
            spares: 0,
            reconfigure: 0,
            max_persist_ms: 5,
            in_flight_slots: 4,
            absent: None,
            kill: None,
            compact_every: Some(32),
            max_message_bytes: 1 << 20,
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
    /// Membership changes applied (by the first replica to apply each).
    pub reconfigurations: u64,
}

//! Read-only view of protocol-internal state for verification (feature `model`; not a
//! stable API). Lets simulators and model checkers check the paper's global invariants
//! (G3–G5) across real replicas, not only client-visible outcomes.

use alloc::vec::Vec;

use crate::proposal::Proposal;
use crate::recorder::Register;

/// A proposal: priority, value key, and a digest of the value bytes (keys can collide
/// when a crash loses the incarnation record, so identity includes the bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProposalView {
    /// Priority; `u64::MAX` is the leader's.
    pub priority: u64,
    /// Origin replica of the value.
    pub origin: u32,
    /// Origin incarnation.
    pub incarnation: u64,
    /// Origin batch sequence.
    pub seq: u64,
    /// FNV-1a of the value bytes.
    pub digest: u64,
}

impl ProposalView {
    /// Identity of the value (independent of priority).
    pub fn value(&self) -> (u32, u64, u64, u64) {
        (self.origin, self.incarnation, self.seq, self.digest)
    }
}

impl From<&Proposal> for ProposalView {
    fn from(p: &Proposal) -> Self {
        let digest = p
            .value
            .iter()
            .fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
        Self {
            priority: p.priority,
            origin: p.key.origin.0,
            incarnation: p.key.incarnation,
            seq: p.key.seq,
            digest,
        }
    }
}

/// One recorder register `(S, Fc, Ac, Ap)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegisterView {
    /// Current step.
    pub step: u32,
    /// First value of the current step.
    pub first: Option<ProposalView>,
    /// Maximum value of the current step.
    pub agg: Option<ProposalView>,
    /// Maximum value of the previous step.
    pub prev_agg: Option<ProposalView>,
}

impl From<&Register> for RegisterView {
    fn from(r: &Register) -> Self {
        Self {
            step: r.step,
            first: r.first.as_ref().map(Into::into),
            agg: r.agg.as_ref().map(Into::into),
            prev_agg: r.prev_agg.as_ref().map(Into::into),
        }
    }
}

/// Snapshot of a replica's protocol state.
#[derive(Clone, Debug, Default)]
pub struct Inspection {
    /// Live recorder registers by slot.
    pub registers: Vec<(u64, RegisterView)>,
    /// Decisions this replica's proposers reached: slot, value and the step decided at.
    pub decided_here: Vec<(u64, ProposalView, u32)>,
    /// Every decision this replica knows: slot and value.
    pub decided: Vec<(u64, ProposalView)>,
    /// Batches this replica proposed that are not yet committed.
    pub local_batches: usize,
    /// Suspected entry combinations this replica has run into since it started.
    pub entries: EntryProbes,
}

/// How often a replica ran into each entry combination suspected reachable in #187 (counted,
/// not asserted).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EntryProbes {
    /// D1: an own entry newly both handed (or about to be handed) to the target and in a
    /// local batch in a slot.
    pub d1: u64,
    /// D2: an own entry a lost slot queued again while it was already queued or handed on.
    pub d2: u64,
    /// Own entries handed on again while still waiting on an earlier hand-off, restarting
    /// their hedge clock (D2's consequence).
    pub d2_restarts: u64,
    /// D3: an own entry of this incarnation handed to this replica by another member.
    pub d3: u64,
    /// D4: on installing a snapshot, an entry of a batch released from its slot that another
    /// local batch still proposes in a slot.
    pub d4: u64,
}

impl core::ops::AddAssign for EntryProbes {
    fn add_assign(&mut self, o: Self) {
        self.d1 += o.d1;
        self.d2 += o.d2;
        self.d2_restarts += o.d2_restarts;
        self.d3 += o.d3;
        self.d4 += o.d4;
    }
}

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
}

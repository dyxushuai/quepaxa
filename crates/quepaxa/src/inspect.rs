//! Read-only view of protocol-internal state for verification (feature `model`; not a
//! stable API). Lets simulators and model checkers check the paper's global invariants
//! (G3–G5) across real replicas, not only client-visible outcomes.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use bytes::{BufMut, Bytes};

use crate::batch::{self, DataId};
use crate::crc::crc32c;
use crate::durable::Record;
use crate::proposal::{Proposal, ValueKey};
use crate::recorder::Register;
use crate::types::{NodeId, Slot};

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
    /// D2, own half: an own entry a lost slot queued again while it was already queued or
    /// handed on (intended: the target decided the slot without it).
    pub d2_own: u64,
    /// D2, other half: another member's entry a lost slot handed back from a batch handed on
    /// when stepping aside (a bug: it went to the target then).
    pub d2_foreign: u64,
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
        self.d2_own += o.d2_own;
        self.d2_foreign += o.d2_foreign;
        self.d2_restarts += o.d2_restarts;
        self.d3 += o.d3;
        self.d4 += o.d4;
    }
}

/// Identity of a data batch (`(origin, incarnation, seq)`), for G6 custody checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DataIdView {
    /// Origin replica of the data batch.
    pub origin: u32,
    /// Origin incarnation.
    pub incarnation: u64,
    /// Origin batch sequence.
    pub seq: u64,
}

impl From<DataId> for DataIdView {
    fn from(id: DataId) -> Self {
        Self { origin: id.origin.0, incarnation: id.incarnation, seq: id.seq }
    }
}

impl From<DataIdView> for DataId {
    fn from(id: DataIdView) -> Self {
        Self { origin: NodeId(id.origin), incarnation: id.incarnation, seq: id.seq }
    }
}

/// Durable custody view of one replica's disk, replayed in record order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiskCustody {
    /// First slot not covered by a durable `Compacted` record (`0` if never compacted).
    pub floor: u64,
    /// Data batch ids present in durable `Data` records on this disk.
    pub data: BTreeSet<DataIdView>,
    /// Referenced data batch ids of each durable `Decided` slot on this disk.
    pub decided: BTreeMap<u64, Vec<DataIdView>>,
    /// Referenced data batch ids of proposal values seen in durable `Recorder` or
    /// `Decided` records, keyed by [`ProposalView::value`].
    pub value_refs: BTreeMap<(u32, u64, u64, u64), Vec<DataIdView>>,
    records_seen: usize,
    data_meta: BTreeMap<DataIdView, (u32, u32)>,
}

impl DiskCustody {
    /// Applies newly appended durable `records` in order, checking G6a for every
    /// `Recorder` record against `Data` records already seen on this disk.
    ///
    /// # Errors
    ///
    /// Returns an error naming the record's zero-based disk index if a record fails to
    /// decode or breaks G6a (missing or mismatched earlier `Data` record).
    pub fn apply(&mut self, records: &[Bytes]) -> Result<(), String> {
        for bytes in records {
            let index = self.records_seen;
            self.records_seen += 1;
            let rec = Record::decode(bytes)
                .map_err(|e| format!("record {index}: corrupt durable record ({e:?})"))?;
            match rec {
                Record::Data { id, bytes } => {
                    let view: DataIdView = id.into();
                    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
                    let crc = crc32c(&bytes);
                    self.data.insert(view);
                    self.data_meta.entry(view).or_insert((len, crc));
                }
                Record::Recorder { slot, reg } => {
                    for p in [&reg.first, &reg.agg, &reg.prev_agg].into_iter().flatten() {
                        let refs = batch::refs(&p.value);
                        if !refs.is_empty() {
                            let mut ids = Vec::with_capacity(refs.len());
                            for r in refs {
                                let id: DataIdView = r.id.into();
                                match self.data_meta.get(&id) {
                                    None => {
                                        return Err(format!(
                                            "record {index}: recorder for slot {} references {id:?} with no earlier Data record",
                                            slot.0
                                        ));
                                    }
                                    Some(&(len, crc)) if len != r.len || crc != r.crc => {
                                        return Err(format!(
                                            "record {index}: recorder for slot {} references {id:?} whose earlier Data record disagrees on len/crc",
                                            slot.0
                                        ));
                                    }
                                    Some(_) => ids.push(id),
                                }
                            }
                            self.value_refs.entry(ProposalView::from(p).value()).or_insert(ids);
                        }
                    }
                }
                Record::Decided { slot, proposal } => {
                    let refs = batch::refs(&proposal.value);
                    let mut ids = Vec::with_capacity(refs.len());
                    for r in refs {
                        let id: DataIdView = r.id.into();
                        if let Some(&(len, crc)) = self.data_meta.get(&id)
                            && (len != r.len || crc != r.crc)
                        {
                            return Err(format!(
                                "record {index}: decided slot {} references {id:?} whose Data record disagrees on len/crc",
                                slot.0
                            ));
                        }
                        ids.push(id);
                    }
                    if !ids.is_empty() {
                        self.value_refs
                            .entry(ProposalView::from(&proposal).value())
                            .or_insert_with(|| ids.clone());
                    }
                    self.decided.insert(slot.0, ids);
                }
                Record::Compacted { through, .. } => {
                    self.floor = self.floor.max(through.0.saturating_add(1));
                }
                Record::Incarnation(_) | Record::Reservation { .. } => {}
            }
        }
        Ok(())
    }
}

/// Replays a replica's durable `records` in order, checking G6a (every `Recorder`
/// record's references have a matching `Data` record earlier on disk) and returning the
/// durable floor, held data ids and decided-slot references for G6b.
///
/// # Errors
///
/// Returns an error naming the offending record's zero-based index if a record fails to
/// decode or breaks G6a.
pub fn custody(records: &[Bytes]) -> Result<DiskCustody, String> {
    let mut state = DiskCustody::default();
    state.apply(records)?;
    Ok(state)
}

/// Encodes a batch value carrying `ENTRY_REF` items for `refs` (each `(id, content)`), for
/// unit-testing G6 custody checks on hand-built record lists.
pub fn ref_batch(refs: &[(DataIdView, &[u8])]) -> Bytes {
    let mut out = Vec::with_capacity(4 + refs.len() * 33);
    out.put_u32_le(u32::try_from(refs.len()).unwrap_or(u32::MAX));
    for &(id, bytes) in refs {
        out.put_u32_le(id.origin);
        out.put_u64_le(id.incarnation);
        out.put_u64_le(id.seq);
        out.put_u8(3);
        out.put_u32_le(8);
        out.put_u32_le(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
        out.put_u32_le(crc32c(bytes));
    }
    Bytes::from(out)
}

/// Encodes a durable `Incarnation` record, for unit-testing G6 custody checks.
pub fn incarnation_record(incarnation: u64) -> Bytes {
    Record::Incarnation(incarnation).encode()
}

/// Encodes a durable `Data` record, for unit-testing G6 custody checks.
pub fn data_record(id: DataIdView, bytes: Bytes) -> Bytes {
    Record::Data { id: id.into(), bytes }.encode()
}

/// Encodes a durable `Recorder` record for `slot` with the given register proposal values,
/// for unit-testing G6 custody checks.
pub fn recorder_record(
    slot: u64,
    first: Option<Bytes>,
    agg: Option<Bytes>,
    prev_agg: Option<Bytes>,
) -> Bytes {
    let prop = |value: Bytes| Proposal {
        priority: 1,
        key: ValueKey { origin: NodeId(0), incarnation: 1, seq: slot },
        value,
    };
    let reg = Register { step: 4, first: first.map(prop), agg: agg.map(prop), prev_agg: prev_agg.map(prop) };
    Record::Recorder { slot: Slot(slot), reg }.encode()
}

/// Encodes a durable `Decided` record for `slot` and `value`, for unit-testing G6 custody
/// checks.
pub fn decided_record(slot: u64, value: Bytes) -> Bytes {
    let proposal =
        Proposal { priority: 1, key: ValueKey { origin: NodeId(0), incarnation: 1, seq: slot }, value };
    Record::Decided { slot: Slot(slot), proposal }.encode()
}

/// Encodes a durable `Compacted` record through `through`, for unit-testing G6 custody
/// checks.
pub fn compacted_record(through: u64) -> Bytes {
    Record::Compacted { through: Slot(through), core_state: Bytes::new() }.encode()
}

/// Returns [`ProposalView::value`] for the proposal encoded by [`recorder_record`] and
/// [`decided_record`], for unit-testing G6b against `History::decided`.
pub fn proposal_value(slot: u64, value: Bytes) -> (u32, u64, u64, u64) {
    let p = Proposal { priority: 1, key: ValueKey { origin: NodeId(0), incarnation: 1, seq: slot }, value };
    ProposalView::from(&p).value()
}

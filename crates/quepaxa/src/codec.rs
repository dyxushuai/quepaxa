//! Canonical wire encoding of network messages (spec §7).
//!
//! Little-endian, fixed-width integers, `u32` length prefixes, zero-copy payloads.
//! Every length is checked against the remaining input and the configured limit
//! before anything is allocated.

use alloc::vec::Vec;
use bytes::{BufMut, Bytes};

use crate::batch::DataId;
use crate::proposal::{Proposal, ValueKey};
use crate::types::{NodeId, Slot, Step};

pub(crate) const MAGIC: [u8; 2] = *b"QP";
pub(crate) const WIRE_VERSION: u8 = 2;

const KIND_RECORD_REQUEST: u8 = 1;
const KIND_RECORD_REPLY: u8 = 2;
const KIND_DECIDED: u8 = 3;
const KIND_CATCH_UP: u8 = 4;
const KIND_ENTRIES: u8 = 5;
const KIND_FORWARD: u8 = 6;
const KIND_COMPACTED: u8 = 7;
const KIND_READ_QUERY: u8 = 8;
const KIND_READ_REPLY: u8 = 9;
const KIND_FORWARD_ACK: u8 = 10;
const KIND_RTT_PROBE: u8 = 11;
const KIND_RTT_REPLY: u8 = 12;
const KIND_DATA: u8 = 13;
const KIND_FETCH: u8 = 14;

const VALUE_INLINE: u8 = 0;

const NIL: u8 = 0;
const PRESENT: u8 = 1;
const ELIDED: u8 = 2;

/// Fields every message carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Envelope {
    pub cluster_id: [u8; 16],
    pub config_epoch: u64,
    pub from: NodeId,
    pub to: NodeId,
    pub commit_index: u64,
}

/// A proposal as it appears in a reply: its value may be elided when the requester
/// already holds it (same key as the request's proposal).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(crate) struct WireProposal {
    pub priority: u64,
    pub key: ValueKey,
    pub value: Option<Bytes>,
}

impl WireProposal {
    /// Restores an elided value from the request's proposal; `None` if the reply refers
    /// to a value the requester cannot have (protocol violation by the peer).
    pub fn resolve(self, request: &Proposal) -> Option<Proposal> {
        let value = match self.value {
            Some(v) => v,
            None if self.key == request.key => request.value.clone(),
            None => return None,
        };
        Some(Proposal { priority: self.priority, key: self.key, value })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(crate) enum Body {
    RecordRequest {
        slot: Slot,
        step: Step,
        proposal: Proposal,
    },
    RecordReply {
        slot: Slot,
        req_step: Step,
        step: Step,
        first: Option<WireProposal>,
        prev_agg: Option<WireProposal>,
    },
    /// A slot's decided value by its priority and key, broadcast by the decider or sent
    /// instead of a record reply. The receiver resolves the value from what it holds under
    /// that key, else catches up from the sender (a key names exactly one value).
    Decided {
        slot: Slot,
        priority: u64,
        key: ValueKey,
    },
    /// Asks a peer for decided slots starting at `from`.
    CatchUp {
        from: Slot,
    },
    /// Contiguous decided slots starting at `first`, with the content of the data batches
    /// their values reference, so that a page is self-contained.
    Entries {
        first: Slot,
        proposals: Vec<Proposal>,
        content: Vec<(DataId, Bytes)>,
    },
    /// Entries a follower hands to the leader (an encoded batch), numbered by the sender so
    /// the receiver can acknowledge it; a forward sent again keeps its number.
    Forward {
        seq: u64,
        batch: Bytes,
    },
    /// The sender took the `Forward`s numbered `seqs` (those since its last acknowledgement):
    /// it is alive, whatever its disk is doing. Sent before any write, it reports no state.
    ForwardAck {
        seqs: Vec<u64>,
    },
    /// The sender no longer has slots up to `through`; fetch its snapshot instead.
    Compacted {
        through: Slot,
    },
    /// Asks a recorder for its high-water mark, for read round `round` of the asker's
    /// `incarnation`.
    ReadQuery {
        incarnation: u64,
        round: u64,
    },
    /// The sender holds no state for any slot at or after `high`.
    ReadReply {
        incarnation: u64,
        round: u64,
        high: Slot,
    },
    /// Leader placement: the sender's incarnation and clock when sent, the smoothed commit
    /// latency of its own entries (0: none yet), and its smoothed round trip to each member
    /// it has a fresh one for, in nanoseconds.
    RttProbe {
        incarnation: u64,
        sent: u64,
        commit: u64,
        table: Vec<(NodeId, u64)>,
    },
    /// Echoes an `RttProbe`'s `incarnation` and `sent`.
    RttReply {
        incarnation: u64,
        sent: u64,
    },
    /// A data batch's content (an encoded batch of entries), from its origin or a holder.
    Data {
        id: DataId,
        bytes: Bytes,
    },
    /// Asks a holder for the content of data batches that values of `slot` reference. The
    /// slot lets a holder that compacted it answer `Compacted` (DEV-3).
    Fetch {
        slot: Slot,
        ids: Vec<DataId>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Message {
    pub env: Envelope,
    pub body: Body,
}

#[cfg(test)]
impl Body {
    /// The announcement of `p` decided in `slot`.
    pub fn decided(slot: Slot, p: Proposal) -> Self {
        Body::Decided { slot, priority: p.priority, key: p.key }
    }
}

impl Body {
    fn largest_value(&self) -> usize {
        let wire = |p: &Option<WireProposal>| p.as_ref().and_then(|p| p.value.as_ref()).map_or(0, Bytes::len);
        match self {
            Body::RecordRequest { proposal, .. } => proposal.value.len(),
            Body::RecordReply { first, prev_agg, .. } => wire(first).max(wire(prev_agg)),
            Body::Entries { proposals, content, .. } => {
                let values = proposals.iter().map(|p| p.value.len());
                values.chain(content.iter().map(|(_, b)| b.len())).max().unwrap_or(0)
            }
            Body::Forward { batch, .. } => batch.len(),
            Body::Data { bytes, .. } => bytes.len(),
            Body::Decided { .. }
            | Body::Fetch { .. }
            | Body::CatchUp { .. }
            | Body::Compacted { .. }
            | Body::ReadQuery { .. }
            | Body::ReadReply { .. }
            | Body::RttProbe { .. }
            | Body::RttReply { .. }
            | Body::ForwardAck { .. } => 0,
        }
    }
}

/// Traffic class of an encoded message, for transports that separate streams to avoid
/// head-of-line blocking (e.g. TCP per peer). Keeping all consensus messages on one stream
/// preserves their relative order while keeping small liveness probes off payload-bearing
/// connections and bulk content off the consensus path.
pub(crate) fn outbound_class(bytes: &[u8]) -> crate::api::OutboundClass {
    use crate::api::OutboundClass;
    if bytes.len() < 4 || bytes[0..2] != MAGIC || bytes[2] != WIRE_VERSION {
        return OutboundClass::Consensus;
    }
    match bytes[3] {
        KIND_FORWARD_ACK | KIND_RTT_PROBE | KIND_RTT_REPLY => OutboundClass::Control,
        KIND_DATA | KIND_ENTRIES => OutboundClass::Bulk,
        _ => OutboundClass::Consensus,
    }
}

/// What the receiver expects from every message.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(crate) struct WireCtx {
    pub cluster_id: [u8; 16],
    pub config_epoch: u64,
    pub max_message_bytes: u32,
    /// Largest proposal value (encoded batch) accepted.
    pub max_value_bytes: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecodeError {
    TooLarge,
    Truncated,
    BadMagic,
    BadVersion,
    WrongCluster,
    WrongEpoch,
    UnknownKind,
    BadTag,
    Trailing,
    BadChecksum,
}

/// Builds a reply proposal, eliding its value when it is the request's own proposal.
pub(crate) fn reply_proposal(p: &Proposal, request_key: ValueKey) -> WireProposal {
    let value = (p.key != request_key).then(|| p.value.clone());
    WireProposal { priority: p.priority, key: p.key, value }
}

impl Message {
    pub fn encode(&self) -> Bytes {
        // 44-byte header + body framing: 128 covers RecordRequest, RecordReply and Decided
        // without reallocating.
        let mut out = Vec::with_capacity(128 + self.payload_len());
        out.put_slice(&MAGIC);
        out.put_u8(WIRE_VERSION);
        out.put_u8(match self.body {
            Body::RecordRequest { .. } => KIND_RECORD_REQUEST,
            Body::RecordReply { .. } => KIND_RECORD_REPLY,
            Body::Decided { .. } => KIND_DECIDED,
            Body::CatchUp { .. } => KIND_CATCH_UP,
            Body::Entries { .. } => KIND_ENTRIES,
            Body::Forward { .. } => KIND_FORWARD,
            Body::Compacted { .. } => KIND_COMPACTED,
            Body::ReadQuery { .. } => KIND_READ_QUERY,
            Body::ReadReply { .. } => KIND_READ_REPLY,
            Body::ForwardAck { .. } => KIND_FORWARD_ACK,
            Body::RttProbe { .. } => KIND_RTT_PROBE,
            Body::RttReply { .. } => KIND_RTT_REPLY,
            Body::Data { .. } => KIND_DATA,
            Body::Fetch { .. } => KIND_FETCH,
        });
        let e = &self.env;
        out.put_slice(&e.cluster_id);
        out.put_u64_le(e.config_epoch);
        out.put_u32_le(e.from.0);
        out.put_u32_le(e.to.0);
        out.put_u64_le(e.commit_index);
        match &self.body {
            Body::RecordRequest { slot, step, proposal } => {
                out.put_u64_le(slot.0);
                out.put_u32_le(*step);
                put_head(&mut out, proposal.priority, proposal.key);
                put_value(&mut out, &proposal.value);
            }
            Body::RecordReply { slot, req_step, step, first, prev_agg } => {
                out.put_u64_le(slot.0);
                out.put_u32_le(*req_step);
                out.put_u32_le(*step);
                put_wire_proposal(&mut out, first.as_ref());
                put_wire_proposal(&mut out, prev_agg.as_ref());
            }
            Body::Decided { slot, priority, key } => {
                out.put_u64_le(slot.0);
                put_head(&mut out, *priority, *key);
            }
            Body::CatchUp { from } => out.put_u64_le(from.0),
            Body::Entries { first, proposals, content } => {
                out.put_u64_le(first.0);
                out.put_u32_le(proposals.len() as u32);
                for p in proposals {
                    put_proposal(&mut out, p);
                }
                out.put_u32_le(content.len() as u32);
                for (id, bytes) in content {
                    put_data_id(&mut out, *id);
                    put_value(&mut out, bytes);
                }
            }
            Body::Forward { seq, batch } => {
                out.put_u64_le(*seq);
                put_value(&mut out, batch);
            }
            Body::ForwardAck { seqs } => {
                out.put_u32_le(seqs.len() as u32);
                for seq in seqs {
                    out.put_u64_le(*seq);
                }
            }
            Body::Compacted { through } => out.put_u64_le(through.0),
            Body::ReadQuery { incarnation, round } => {
                out.put_u64_le(*incarnation);
                out.put_u64_le(*round);
            }
            Body::ReadReply { incarnation, round, high } => {
                out.put_u64_le(*incarnation);
                out.put_u64_le(*round);
                out.put_u64_le(high.0);
            }
            Body::RttProbe { incarnation, sent, commit, table } => {
                out.put_u64_le(*incarnation);
                out.put_u64_le(*sent);
                out.put_u64_le(*commit);
                out.put_u32_le(table.len() as u32);
                for (node, rtt) in table {
                    out.put_u32_le(node.0);
                    out.put_u64_le(*rtt);
                }
            }
            Body::RttReply { incarnation, sent } => {
                out.put_u64_le(*incarnation);
                out.put_u64_le(*sent);
            }
            Body::Data { id, bytes } => {
                put_data_id(&mut out, *id);
                put_value(&mut out, bytes);
            }
            Body::Fetch { slot, ids } => {
                out.put_u64_le(slot.0);
                out.put_u32_le(ids.len() as u32);
                for id in ids {
                    put_data_id(&mut out, *id);
                }
            }
        }
        Bytes::from(out)
    }

    fn payload_len(&self) -> usize {
        let len = |p: &Option<WireProposal>| p.as_ref().and_then(|p| p.value.as_ref()).map_or(0, Bytes::len);
        match &self.body {
            Body::RecordRequest { proposal, .. } => proposal.value.len(),
            Body::RecordReply { first, prev_agg, .. } => len(first) + len(prev_agg),
            Body::Decided { .. }
            | Body::CatchUp { .. }
            | Body::Compacted { .. }
            | Body::ReadQuery { .. }
            | Body::ReadReply { .. }
            | Body::RttReply { .. } => 0,
            Body::ForwardAck { seqs } => 4 + 8 * seqs.len(),
            Body::RttProbe { table, .. } => 28 + 12 * table.len(),
            Body::Forward { batch, .. } => batch.len(),
            Body::Entries { proposals, content, .. } => {
                let values: usize = proposals.iter().map(|p| p.value.len() + 33).sum();
                values + content.iter().map(|(_, b)| b.len() + DATA_ID_LEN + 5).sum::<usize>()
            }
            Body::Data { bytes, .. } => DATA_ID_LEN + 5 + bytes.len(),
            Body::Fetch { ids, .. } => 12 + DATA_ID_LEN * ids.len(),
        }
    }

    pub fn decode(buf: &Bytes, ctx: &WireCtx) -> Result<Self, DecodeError> {
        if buf.len() > ctx.max_message_bytes as usize {
            return Err(DecodeError::TooLarge);
        }
        let mut r = Reader::new(buf);
        if r.take(2)? != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        if r.u8()? != WIRE_VERSION {
            return Err(DecodeError::BadVersion);
        }
        let kind = r.u8()?;
        let mut cluster_id = [0u8; 16];
        cluster_id.copy_from_slice(r.take(16)?);
        if cluster_id != ctx.cluster_id {
            return Err(DecodeError::WrongCluster);
        }
        let config_epoch = r.u64()?;
        if config_epoch != ctx.config_epoch {
            return Err(DecodeError::WrongEpoch);
        }
        let env = Envelope {
            cluster_id,
            config_epoch,
            from: NodeId(r.u32()?),
            to: NodeId(r.u32()?),
            commit_index: r.u64()?,
        };
        let body = match kind {
            KIND_RECORD_REQUEST => {
                let slot = Slot(r.u64()?);
                let step = r.u32()?;
                Body::RecordRequest { slot, step, proposal: r.proposal()? }
            }
            KIND_RECORD_REPLY => Body::RecordReply {
                slot: Slot(r.u64()?),
                req_step: r.u32()?,
                step: r.u32()?,
                first: r.wire_proposal()?,
                prev_agg: r.wire_proposal()?,
            },
            KIND_DECIDED => {
                let slot = Slot(r.u64()?);
                let (priority, key) = r.head()?;
                Body::Decided { slot, priority, key }
            }
            KIND_CATCH_UP => Body::CatchUp { from: Slot(r.u64()?) },
            KIND_ENTRIES => {
                let first = Slot(r.u64()?);
                let n = r.u32()? as usize;
                // Each proposal takes at least 33 bytes, bounding the allocation by input size.
                let mut proposals = Vec::with_capacity(n.min(buf.len() / 33));
                for _ in 0..n {
                    proposals.push(r.proposal()?);
                }
                let n = r.u32()? as usize;
                // Each batch takes at least 25 bytes (its id and an empty value).
                let mut content = Vec::with_capacity(n.min(buf.len() / (DATA_ID_LEN + 5)));
                for _ in 0..n {
                    content.push((r.data_id()?, r.value()?));
                }
                Body::Entries { first, proposals, content }
            }
            KIND_FORWARD => Body::Forward { seq: r.u64()?, batch: r.value()? },
            KIND_FORWARD_ACK => {
                let n = r.u32()? as usize;
                // Each number takes 8 bytes, bounding the allocation by input size.
                let mut seqs = Vec::with_capacity(n.min(buf.len() / 8));
                for _ in 0..n {
                    seqs.push(r.u64()?);
                }
                Body::ForwardAck { seqs }
            }
            KIND_COMPACTED => Body::Compacted { through: Slot(r.u64()?) },
            KIND_READ_QUERY => Body::ReadQuery { incarnation: r.u64()?, round: r.u64()? },
            KIND_READ_REPLY => {
                Body::ReadReply { incarnation: r.u64()?, round: r.u64()?, high: Slot(r.u64()?) }
            }
            KIND_RTT_PROBE => {
                let incarnation = r.u64()?;
                let sent = r.u64()?;
                let commit = r.u64()?;
                let n = r.u32()? as usize;
                // Each row takes 12 bytes, bounding the allocation by input size.
                let mut table = Vec::with_capacity(n.min(buf.len() / 12));
                for _ in 0..n {
                    table.push((NodeId(r.u32()?), r.u64()?));
                }
                Body::RttProbe { incarnation, sent, commit, table }
            }
            KIND_RTT_REPLY => Body::RttReply { incarnation: r.u64()?, sent: r.u64()? },
            KIND_DATA => Body::Data { id: r.data_id()?, bytes: r.value()? },
            KIND_FETCH => {
                let slot = Slot(r.u64()?);
                let n = r.u32()? as usize;
                let mut ids = Vec::with_capacity(n.min(buf.len() / DATA_ID_LEN));
                for _ in 0..n {
                    ids.push(r.data_id()?);
                }
                Body::Fetch { slot, ids }
            }
            _ => return Err(DecodeError::UnknownKind),
        };
        r.finish()?;
        if body.largest_value() > ctx.max_value_bytes as usize {
            return Err(DecodeError::TooLarge);
        }
        Ok(Self { env, body })
    }
}

pub(crate) fn put_head(out: &mut Vec<u8>, priority: u64, key: ValueKey) {
    out.put_u64_le(priority);
    out.put_u32_le(key.origin.0);
    out.put_u64_le(key.incarnation);
    out.put_u64_le(key.seq);
}

/// Encoded size of a [`DataId`].
const DATA_ID_LEN: usize = 20;

pub(crate) fn put_data_id(out: &mut Vec<u8>, id: DataId) {
    out.put_u32_le(id.origin.0);
    out.put_u64_le(id.incarnation);
    out.put_u64_le(id.seq);
}

pub(crate) fn put_value(out: &mut Vec<u8>, value: &Bytes) {
    out.put_u8(VALUE_INLINE);
    // Lengths beyond u32 are rejected earlier by `max_batch_bytes`.
    out.put_u32_le(value.len() as u32);
    out.put_slice(value);
}

pub(crate) fn put_proposal(out: &mut Vec<u8>, p: &Proposal) {
    put_head(out, p.priority, p.key);
    put_value(out, &p.value);
}

pub(crate) fn put_opt_proposal(out: &mut Vec<u8>, p: Option<&Proposal>) {
    match p {
        None => out.put_u8(NIL),
        Some(p) => {
            out.put_u8(PRESENT);
            put_proposal(out, p);
        }
    }
}

fn put_wire_proposal(out: &mut Vec<u8>, p: Option<&WireProposal>) {
    match p {
        None => out.put_u8(NIL),
        Some(p) => {
            out.put_u8(if p.value.is_some() { PRESENT } else { ELIDED });
            put_head(out, p.priority, p.key);
            if let Some(v) = &p.value {
                put_value(out, v);
            }
        }
    }
}

pub(crate) struct Reader<'a> {
    buf: &'a Bytes,
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a Bytes) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn finish(&self) -> Result<(), DecodeError> {
        if self.pos == self.buf.len() { Ok(()) } else { Err(DecodeError::Trailing) }
    }

    pub fn proposal(&mut self) -> Result<Proposal, DecodeError> {
        let (priority, key) = self.head()?;
        Ok(Proposal { priority, key, value: self.value()? })
    }

    pub fn peek_u8(&self) -> Result<u8, DecodeError> {
        self.buf.get(self.pos).copied().ok_or(DecodeError::Truncated)
    }

    pub fn opt_proposal(&mut self) -> Result<Option<Proposal>, DecodeError> {
        match self.u8()? {
            NIL => Ok(None),
            PRESENT => Ok(Some(self.proposal()?)),
            _ => Err(DecodeError::BadTag),
        }
    }

    pub fn take(&mut self, n: usize) -> Result<&[u8], DecodeError> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len()).ok_or(DecodeError::Truncated)?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        let (b, _) = self.take(4)?.as_chunks::<4>();
        Ok(u32::from_le_bytes(b[0]))
    }

    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        let (b, _) = self.take(8)?.as_chunks::<8>();
        Ok(u64::from_le_bytes(b[0]))
    }

    pub fn head(&mut self) -> Result<(u64, ValueKey), DecodeError> {
        let priority = self.u64()?;
        let key = ValueKey { origin: NodeId(self.u32()?), incarnation: self.u64()?, seq: self.u64()? };
        Ok((priority, key))
    }

    pub fn data_id(&mut self) -> Result<DataId, DecodeError> {
        Ok(DataId { origin: NodeId(self.u32()?), incarnation: self.u64()?, seq: self.u64()? })
    }

    /// Zero-copy: the value shares the input buffer.
    pub fn value(&mut self) -> Result<Bytes, DecodeError> {
        if self.u8()? != VALUE_INLINE {
            return Err(DecodeError::BadTag);
        }
        let len = self.u32()? as usize;
        let start = self.pos;
        self.take(len)?;
        Ok(self.buf.slice(start..start + len))
    }

    fn wire_proposal(&mut self) -> Result<Option<WireProposal>, DecodeError> {
        let tag = self.u8()?;
        if tag == NIL {
            return Ok(None);
        }
        if tag != PRESENT && tag != ELIDED {
            return Err(DecodeError::BadTag);
        }
        let (priority, key) = self.head()?;
        let value = if tag == PRESENT { Some(self.value()?) } else { None };
        Ok(Some(WireProposal { priority, key, value }))
    }
}

#[cfg(test)]
#[path = "codec_tests.rs"]
mod tests;

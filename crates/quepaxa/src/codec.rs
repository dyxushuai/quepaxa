//! Canonical wire encoding of network messages (spec §7).
//!
//! Little-endian, fixed-width integers, `u32` length prefixes, zero-copy payloads.
//! Every length is checked against the remaining input and the configured limit
//! before anything is allocated.

use alloc::vec::Vec;
use bytes::{BufMut, Bytes};

use crate::proposal::{Proposal, ValueKey};
use crate::types::{NodeId, Slot, Step};

pub(crate) const MAGIC: [u8; 2] = *b"QP";
pub(crate) const WIRE_VERSION: u8 = 1;

const KIND_RECORD_REQUEST: u8 = 1;
const KIND_RECORD_REPLY: u8 = 2;
const KIND_DECIDED: u8 = 3;

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
        Some(Proposal {
            priority: self.priority,
            key: self.key,
            value,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// A slot's decided value, broadcast by the decider or sent instead of a record reply.
    Decided { slot: Slot, proposal: Proposal },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Message {
    pub env: Envelope,
    pub body: Body,
}

/// What the receiver expects from every message.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WireCtx {
    pub cluster_id: [u8; 16],
    pub config_epoch: u64,
    pub max_message_bytes: u32,
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
    WireProposal {
        priority: p.priority,
        key: p.key,
        value,
    }
}

impl Message {
    pub fn encode(&self) -> Bytes {
        let mut out = Vec::with_capacity(64 + self.payload_len());
        out.put_slice(&MAGIC);
        out.put_u8(WIRE_VERSION);
        out.put_u8(match self.body {
            Body::RecordRequest { .. } => KIND_RECORD_REQUEST,
            Body::RecordReply { .. } => KIND_RECORD_REPLY,
            Body::Decided { .. } => KIND_DECIDED,
        });
        let e = &self.env;
        out.put_slice(&e.cluster_id);
        out.put_u64_le(e.config_epoch);
        out.put_u32_le(e.from.0);
        out.put_u32_le(e.to.0);
        out.put_u64_le(e.commit_index);
        match &self.body {
            Body::RecordRequest {
                slot,
                step,
                proposal,
            } => {
                out.put_u64_le(slot.0);
                out.put_u32_le(*step);
                put_head(&mut out, proposal.priority, proposal.key);
                put_value(&mut out, &proposal.value);
            }
            Body::RecordReply {
                slot,
                req_step,
                step,
                first,
                prev_agg,
            } => {
                out.put_u64_le(slot.0);
                out.put_u32_le(*req_step);
                out.put_u32_le(*step);
                put_wire_proposal(&mut out, first.as_ref());
                put_wire_proposal(&mut out, prev_agg.as_ref());
            }
            Body::Decided { slot, proposal } => {
                out.put_u64_le(slot.0);
                put_proposal(&mut out, proposal);
            }
        }
        Bytes::from(out)
    }

    fn payload_len(&self) -> usize {
        let len = |p: &Option<WireProposal>| {
            p.as_ref()
                .and_then(|p| p.value.as_ref())
                .map_or(0, Bytes::len)
        };
        match &self.body {
            Body::RecordRequest { proposal, .. } | Body::Decided { proposal, .. } => {
                proposal.value.len()
            }
            Body::RecordReply {
                first, prev_agg, ..
            } => len(first) + len(prev_agg),
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
                Body::RecordRequest {
                    slot,
                    step,
                    proposal: r.proposal()?,
                }
            }
            KIND_RECORD_REPLY => Body::RecordReply {
                slot: Slot(r.u64()?),
                req_step: r.u32()?,
                step: r.u32()?,
                first: r.wire_proposal()?,
                prev_agg: r.wire_proposal()?,
            },
            KIND_DECIDED => Body::Decided {
                slot: Slot(r.u64()?),
                proposal: r.proposal()?,
            },
            _ => return Err(DecodeError::UnknownKind),
        };
        r.finish()?;
        Ok(Self { env, body })
    }
}

pub(crate) fn put_head(out: &mut Vec<u8>, priority: u64, key: ValueKey) {
    out.put_u64_le(priority);
    out.put_u32_le(key.origin.0);
    out.put_u64_le(key.incarnation);
    out.put_u64_le(key.seq);
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
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(DecodeError::Trailing)
        }
    }

    pub fn proposal(&mut self) -> Result<Proposal, DecodeError> {
        let (priority, key) = self.head()?;
        Ok(Proposal {
            priority,
            key,
            value: self.value()?,
        })
    }

    pub fn opt_proposal(&mut self) -> Result<Option<Proposal>, DecodeError> {
        match self.u8()? {
            NIL => Ok(None),
            PRESENT => Ok(Some(self.proposal()?)),
            _ => Err(DecodeError::BadTag),
        }
    }

    pub fn take(&mut self, n: usize) -> Result<&[u8], DecodeError> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&e| e <= self.buf.len())
            .ok_or(DecodeError::Truncated)?;
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
        let key = ValueKey {
            origin: NodeId(self.u32()?),
            incarnation: self.u64()?,
            seq: self.u64()?,
        };
        Ok((priority, key))
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
        let value = if tag == PRESENT {
            Some(self.value()?)
        } else {
            None
        };
        Ok(Some(WireProposal {
            priority,
            key,
            value,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proposal::tests::prop;

    const CTX: WireCtx = WireCtx {
        cluster_id: [0xAB; 16],
        config_epoch: 7,
        max_message_bytes: 1 << 20,
    };

    fn env() -> Envelope {
        Envelope {
            cluster_id: [0xAB; 16],
            config_epoch: 7,
            from: NodeId(1),
            to: NodeId(2),
            commit_index: 9,
        }
    }

    fn hex(s: &str) -> Vec<u8> {
        let s: Vec<u8> = s.bytes().filter(u8::is_ascii_hexdigit).collect();
        s.chunks(2)
            .map(|c| u8::from_str_radix(core::str::from_utf8(c).unwrap(), 16).unwrap())
            .collect()
    }

    const ENVELOPE_HEX: &str = "5150 01 KK abababababababababababababababab 0700000000000000 01000000 02000000 0900000000000000";

    fn golden(kind: &str, body: &str) -> Vec<u8> {
        hex(&ENVELOPE_HEX.replace("KK", kind).replace(' ', ""))
            .into_iter()
            .chain(hex(body))
            .collect()
    }

    fn request() -> Message {
        let proposal = Proposal {
            priority: 0x0102,
            key: ValueKey {
                origin: NodeId(3),
                incarnation: 4,
                seq: 5,
            },
            value: Bytes::from_static(b"hi"),
        };
        Message {
            env: env(),
            body: Body::RecordRequest {
                slot: Slot(6),
                step: 4,
                proposal,
            },
        }
    }

    #[test]
    fn record_request_matches_golden_bytes_v1() {
        let body = "0600000000000000 04000000 0201000000000000 03000000 0400000000000000 0500000000000000 00 02000000 6869";
        assert_eq!(request().encode().to_vec(), golden("01", body));
    }

    #[test]
    fn record_reply_matches_golden_bytes_v1() {
        let first = WireProposal {
            priority: 1,
            key: ValueKey {
                origin: NodeId(3),
                incarnation: 4,
                seq: 5,
            },
            value: None,
        };
        let msg = Message {
            env: env(),
            body: Body::RecordReply {
                slot: Slot(6),
                req_step: 4,
                step: 5,
                first: Some(first),
                prev_agg: None,
            },
        };
        let body = "0600000000000000 04000000 05000000 02 0100000000000000 03000000 0400000000000000 0500000000000000 00";
        assert_eq!(msg.encode().to_vec(), golden("02", body));
    }

    #[test]
    fn decode_shares_the_input_buffer() {
        let bytes = request().encode();
        let Body::RecordRequest { proposal, .. } = Message::decode(&bytes, &CTX).unwrap().body
        else {
            panic!()
        };
        assert_eq!(proposal.value.as_ptr(), bytes[bytes.len() - 2..].as_ptr());
    }

    #[test]
    fn foreign_cluster_epoch_and_version_are_rejected() {
        let bytes = request().encode();
        let mut other = CTX;
        other.cluster_id = [0; 16];
        assert_eq!(
            Message::decode(&bytes, &other),
            Err(DecodeError::WrongCluster)
        );
        let mut other = CTX;
        other.config_epoch = 8;
        assert_eq!(
            Message::decode(&bytes, &other),
            Err(DecodeError::WrongEpoch)
        );
        let mut v = bytes.to_vec();
        v[2] = 2;
        assert_eq!(
            Message::decode(&Bytes::from(v), &CTX),
            Err(DecodeError::BadVersion)
        );
    }

    #[test]
    fn oversized_length_prefix_is_rejected_without_allocating() {
        let mut v = request().encode().to_vec();
        let n = v.len();
        v[n - 6..n - 2].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            Message::decode(&Bytes::from(v), &CTX),
            Err(DecodeError::Truncated)
        );
    }

    #[test]
    fn message_above_limit_is_rejected() {
        let small = WireCtx {
            max_message_bytes: 10,
            ..CTX
        };
        assert_eq!(
            Message::decode(&request().encode(), &small),
            Err(DecodeError::TooLarge)
        );
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut v = request().encode().to_vec();
        v.push(0);
        assert_eq!(
            Message::decode(&Bytes::from(v), &CTX),
            Err(DecodeError::Trailing)
        );
    }

    #[test]
    fn elided_value_resolves_only_against_the_same_key() {
        let mine = prop(9, 1, 0);
        let elided = reply_proposal(&mine, mine.key);
        assert_eq!(elided.value, None);
        assert_eq!(elided.clone().resolve(&mine), Some(mine.clone()));
        assert_eq!(elided.resolve(&prop(9, 2, 0)), None);
    }

    #[test]
    fn other_proposals_keep_their_value_in_replies() {
        let other = prop(9, 2, 0);
        assert_eq!(
            reply_proposal(&other, prop(1, 1, 0).key).value,
            Some(other.value.clone())
        );
    }

    fn arb_prop() -> impl proptest::strategy::Strategy<Value = Proposal> {
        use proptest::prelude::*;
        (
            any::<u64>(),
            any::<u32>(),
            any::<u64>(),
            any::<u64>(),
            proptest::collection::vec(any::<u8>(), 0..64),
        )
            .prop_map(|(priority, o, i, s, v)| Proposal {
                priority,
                key: ValueKey {
                    origin: NodeId(o),
                    incarnation: i,
                    seq: s,
                },
                value: Bytes::from(v),
            })
    }

    fn arb_wire() -> impl proptest::strategy::Strategy<Value = Option<WireProposal>> {
        use proptest::prelude::*;
        proptest::option::of(
            (arb_prop(), any::<bool>()).prop_map(|(p, elide)| WireProposal {
                priority: p.priority,
                key: p.key,
                value: (!elide).then_some(p.value),
            }),
        )
    }

    proptest::proptest! {
        #[test]
        fn request_round_trips(slot: u64, step: u32, from: u32, to: u32, ci: u64, p in arb_prop()) {
            let env = Envelope { from: NodeId(from), to: NodeId(to), commit_index: ci, ..env() };
            let m = Message { env, body: Body::RecordRequest { slot: Slot(slot), step, proposal: p.clone() } };
            let d = Message::decode(&m.encode(), &CTX).unwrap();
            let Body::RecordRequest { proposal, .. } = &d.body else { panic!() };
            proptest::prop_assert_eq!(&proposal.value, &p.value);
            proptest::prop_assert_eq!(d, m);
        }

        #[test]
        fn reply_round_trips(slot: u64, rs: u32, s: u32, f in arb_wire(), a in arb_wire()) {
            let m = Message { env: env(), body: Body::RecordReply { slot: Slot(slot), req_step: rs, step: s, first: f, prev_agg: a } };
            proptest::prop_assert_eq!(Message::decode(&m.encode(), &CTX).unwrap(), m);
        }
    }

    #[test]
    fn decode_never_panics_on_arbitrary_input() {
        bolero::check!().for_each(|input: &[u8]| {
            let _ = Message::decode(&Bytes::copy_from_slice(input), &CTX);
        });
    }

    #[test]
    fn decode_never_panics_on_mutated_valid_messages() {
        let base = request().encode().to_vec();
        bolero::check!()
            .with_type::<(u16, u8, u8)>()
            .for_each(|&(pos, val, cut)| {
                let mut v = base.clone();
                let i = pos as usize % v.len();
                v[i] = val;
                v.truncate(v.len() - (cut as usize % 4));
                let _ = Message::decode(&Bytes::from(v), &CTX);
            });
    }
}

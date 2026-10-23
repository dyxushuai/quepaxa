//! Durable record format (spec §4): what a replica asks the caller to persist.
//!
//! `"QR"`, format version, kind, payload, then CRC-32C over everything before it.

use alloc::vec::Vec;
use bytes::{BufMut, Bytes};

use crate::codec::{DecodeError, Reader, put_opt_proposal, put_proposal};
use crate::crc::crc32c;
use crate::proposal::Proposal;
use crate::recorder::Register;
use crate::types::Slot;

const MAGIC: [u8; 2] = *b"QR";
const FORMAT_VERSION: u8 = 1;

const KIND_INCARNATION: u8 = 1;
const KIND_RESERVATION: u8 = 3;
const KIND_DECIDED: u8 = 4;
const KIND_COMPACTED: u8 = 5;
/// A recorder register whose `agg` and `prev_agg` may refer to an earlier field instead of
/// repeating its value (on the fast path `agg` is `first`). Kind 2 was an earlier encoding.
const KIND_RECORDER: u8 = 6;

// Field tags of `KIND_RECORDER` beyond the codec's nil/present.
const SAME_AS_FIRST: u8 = 3;
const SAME_AS_AGG: u8 = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Record {
    /// Written first on every start; the new incarnation is the highest seen plus one.
    Incarnation(u64),
    /// A recorder register after a change.
    Recorder { slot: Slot, reg: Register },
    /// The leader may use the top priority for slots up to `through` in this incarnation.
    Reservation { through: Slot },
    /// A slot's decided value.
    Decided { slot: Slot, proposal: Proposal },
    /// Every slot up to `through` is covered by an application snapshot carrying `core_state`.
    Compacted { through: Slot, core_state: Bytes },
}

impl Record {
    pub fn encode(&self) -> Bytes {
        let mut out = Vec::with_capacity(64);
        out.put_slice(&MAGIC);
        out.put_u8(FORMAT_VERSION);
        match self {
            Record::Incarnation(n) => {
                out.put_u8(KIND_INCARNATION);
                out.put_u64_le(*n);
            }
            Record::Recorder { slot, reg } => {
                out.put_u8(KIND_RECORDER);
                out.put_u64_le(slot.0);
                out.put_u32_le(reg.step);
                let (first, agg) = (reg.first.as_ref(), reg.agg.as_ref());
                put_opt_proposal(&mut out, first);
                put_shared(&mut out, agg, &[(SAME_AS_FIRST, first)]);
                put_shared(&mut out, reg.prev_agg.as_ref(), &[(SAME_AS_FIRST, first), (SAME_AS_AGG, agg)]);
            }
            Record::Reservation { through } => {
                out.put_u8(KIND_RESERVATION);
                out.put_u64_le(through.0);
            }
            Record::Decided { slot, proposal } => {
                out.put_u8(KIND_DECIDED);
                out.put_u64_le(slot.0);
                put_proposal(&mut out, proposal);
            }
            Record::Compacted { through, core_state } => {
                out.put_u8(KIND_COMPACTED);
                out.put_u64_le(through.0);
                crate::codec::put_value(&mut out, core_state);
            }
        }
        let crc = crc32c(&out);
        out.put_u32_le(crc);
        Bytes::from(out)
    }

    pub fn decode(buf: &Bytes) -> Result<Self, DecodeError> {
        let body_len = buf.len().checked_sub(4).ok_or(DecodeError::Truncated)?;
        let mut crc = [0u8; 4];
        crc.copy_from_slice(&buf[body_len..]);
        if crc32c(&buf[..body_len]) != u32::from_le_bytes(crc) {
            return Err(DecodeError::BadChecksum);
        }
        let body = buf.slice(..body_len);
        let mut r = Reader::new(&body);
        if r.take(2)? != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        if r.u8()? != FORMAT_VERSION {
            return Err(DecodeError::BadVersion);
        }
        let rec = match r.u8()? {
            KIND_INCARNATION => Record::Incarnation(r.u64()?),
            KIND_RECORDER => {
                let slot = Slot(r.u64()?);
                let step = r.u32()?;
                let first = r.opt_proposal()?;
                let agg = shared(&mut r, &[(SAME_AS_FIRST, &first)])?;
                let prev_agg = shared(&mut r, &[(SAME_AS_FIRST, &first), (SAME_AS_AGG, &agg)])?;
                Record::Recorder { slot, reg: Register { step, first, agg, prev_agg } }
            }
            KIND_RESERVATION => Record::Reservation { through: Slot(r.u64()?) },
            KIND_DECIDED => Record::Decided { slot: Slot(r.u64()?), proposal: r.proposal()? },
            KIND_COMPACTED => Record::Compacted { through: Slot(r.u64()?), core_state: r.value()? },
            _ => return Err(DecodeError::UnknownKind),
        };
        r.finish()?;
        Ok(rec)
    }
}

/// Writes `p` as a tag naming an identical earlier field, or in full.
fn put_shared(out: &mut Vec<u8>, p: Option<&Proposal>, earlier: &[(u8, Option<&Proposal>)]) {
    let same = |e: &Proposal, p: &Proposal| e.priority == p.priority && e.key == p.key && e.value == p.value;
    match p.and_then(|p| earlier.iter().find(|(_, e)| e.is_some_and(|e| same(e, p)))) {
        Some((tag, _)) => out.put_u8(*tag),
        None => put_opt_proposal(out, p),
    }
}

fn shared(r: &mut Reader<'_>, earlier: &[(u8, &Option<Proposal>)]) -> Result<Option<Proposal>, DecodeError> {
    let tag = r.peek_u8()?;
    match earlier.iter().find(|(t, _)| *t == tag) {
        Some((_, Some(p))) => {
            r.u8()?;
            Ok(Some(p.clone()))
        }
        Some((_, None)) => Err(DecodeError::BadTag),
        None => r.opt_proposal(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proposal::tests::prop;

    fn samples() -> [Record; 6] {
        let v = prop(9, 1, 0);
        [
            Record::Incarnation(3),
            Record::Recorder {
                slot: Slot(3),
                reg: Register {
                    step: 5,
                    first: Some(v.clone()),
                    agg: Some(prop(10, 2, 0)),
                    prev_agg: Some(v),
                },
            },
            Record::Recorder {
                slot: Slot(9),
                reg: Register {
                    step: 6,
                    first: Some(prop(4, 1, 0)),
                    agg: Some(prop(8, 2, 1)),
                    prev_agg: None,
                },
            },
            Record::Reservation { through: Slot(40) },
            Record::Decided { slot: Slot(9), proposal: prop(8, 2, 1) },
            Record::Compacted { through: Slot(7), core_state: Bytes::from_static(b"state") },
        ]
    }

    #[test]
    fn every_kind_round_trips() {
        for rec in samples() {
            assert_eq!(Record::decode(&rec.encode()), Ok(rec));
        }
    }

    proptest::proptest! {
        // Any register round-trips, whatever its fields share: equal priority alone, or
        // equal key alone, must not turn a field into a reference to another.
        #[test]
        fn every_register_round_trips(
            fields in proptest::collection::vec(proptest::option::of((1u64..3, 1u32..3, 0u8..2)), 3),
            step in 0u32..100,
        ) {
            let p = |f: Option<(u64, u32, u8)>| f.map(|(prio, origin, v)| Proposal {
                priority: prio,
                key: crate::proposal::ValueKey { origin: crate::types::NodeId(origin), incarnation: 1, seq: 0 },
                value: Bytes::from(alloc::vec![v]),
            });
            let reg = Register { step, first: p(fields[0]), agg: p(fields[1]), prev_agg: p(fields[2]) };
            let rec = Record::Recorder { slot: Slot(1), reg };
            proptest::prop_assert_eq!(Record::decode(&rec.encode()), Ok(rec));
        }
    }

    #[test]
    fn fast_path_registers_store_their_value_once() {
        let v = prop(9, 1, 0);
        let reg =
            Register { step: 4, first: Some(v.clone()), agg: Some(v.clone()), prev_agg: Some(v.clone()) };
        let rec = Record::Recorder { slot: Slot(3), reg };
        let bytes = rec.encode();
        assert_eq!(Record::decode(&bytes), Ok(rec));
        let single = Record::Decided { slot: Slot(3), proposal: v }.encode();
        assert_eq!(bytes.len(), single.len() + 4 + 1 + 1 + 1, "step, one present tag, two references");
    }

    #[test]
    fn an_earlier_recorder_encoding_is_rejected() {
        let mut old = b"QR\x01\x02".to_vec();
        old.extend_from_slice(&9u64.to_le_bytes());
        old.extend_from_slice(&6u32.to_le_bytes());
        old.extend_from_slice(&[0, 0, 0]);
        let crc = crc32c(&old);
        old.extend_from_slice(&crc.to_le_bytes());
        assert!(Record::decode(&Bytes::from(old)).is_err());
    }

    #[test]
    fn a_reference_to_an_absent_field_is_rejected() {
        let mut bad = b"QR\x01\x06".to_vec();
        bad.extend_from_slice(&9u64.to_le_bytes());
        bad.extend_from_slice(&6u32.to_le_bytes());
        bad.extend_from_slice(&[0, SAME_AS_FIRST, 0]);
        let crc = crc32c(&bad);
        bad.extend_from_slice(&crc.to_le_bytes());
        assert_eq!(Record::decode(&Bytes::from(bad)), Err(DecodeError::BadTag));
    }

    #[test]
    fn incarnation_matches_golden_bytes_v1() {
        let mut expected = b"QR\x01\x01".to_vec();
        expected.extend_from_slice(&3u64.to_le_bytes());
        let crc = crc32c(&expected);
        expected.extend_from_slice(&crc.to_le_bytes());
        assert_eq!(Record::Incarnation(3).encode().to_vec(), expected);
    }

    #[test]
    fn any_flipped_bit_is_detected() {
        for rec in samples() {
            let bytes = rec.encode().to_vec();
            for i in 0..bytes.len() * 8 {
                let mut v = bytes.clone();
                v[i / 8] ^= 1 << (i % 8);
                assert!(Record::decode(&Bytes::from(v)).is_err(), "bit {i} of {rec:?}");
            }
        }
    }

    #[test]
    fn torn_record_is_rejected() {
        let bytes = samples()[1].encode();
        for len in 0..bytes.len() {
            assert!(Record::decode(&bytes.slice(..len)).is_err());
        }
    }

    #[test]
    fn decode_never_panics() {
        bolero::check!().for_each(|input: &[u8]| {
            let _ = Record::decode(&Bytes::copy_from_slice(input));
        });
    }
}

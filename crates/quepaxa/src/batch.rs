//! Encoding of the opaque value decided in one slot: a list of proposed entries.

use alloc::vec::Vec;
use bytes::{BufMut, Bytes};

use crate::codec::{DecodeError, Reader};
use crate::types::{NodeId, ProposalId};

const ENTRY_APP: u8 = 0;

/// One application payload committed in the log, tagged with where it was proposed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Replica that accepted the proposal.
    pub origin: NodeId,
    /// Incarnation of that replica when it accepted the proposal.
    pub incarnation: u64,
    /// Identifier returned by [`crate::Replica::propose`] at the origin.
    pub id: ProposalId,
    /// Application bytes.
    pub payload: Bytes,
}

pub(crate) fn encoded_len(e: &Entry) -> usize {
    4 + 8 + 8 + 1 + 4 + e.payload.len()
}

pub(crate) fn encode(entries: &[Entry]) -> Bytes {
    let mut out = Vec::with_capacity(4 + entries.iter().map(encoded_len).sum::<usize>());
    out.put_u32_le(entries.len() as u32);
    for e in entries {
        out.put_u32_le(e.origin.0);
        out.put_u64_le(e.incarnation);
        out.put_u64_le(e.id.0);
        out.put_u8(ENTRY_APP);
        out.put_u32_le(e.payload.len() as u32);
        out.put_slice(&e.payload);
    }
    Bytes::from(out)
}

pub(crate) fn decode(buf: &Bytes) -> Result<Vec<Entry>, DecodeError> {
    let mut r = Reader::new(buf);
    let n = r.u32()? as usize;
    // Each entry takes at least 25 bytes, which bounds the allocation by the input size.
    let mut entries = Vec::with_capacity(n.min(buf.len() / 25));
    for _ in 0..n {
        let origin = NodeId(r.u32()?);
        let incarnation = r.u64()?;
        let id = ProposalId(r.u64()?);
        if r.u8()? != ENTRY_APP {
            return Err(DecodeError::BadTag);
        }
        let len = r.u32()? as usize;
        let start = r.pos();
        r.take(len)?;
        entries.push(Entry {
            origin,
            incarnation,
            id,
            payload: buf.slice(start..start + len),
        });
    }
    r.finish()?;
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn entry(o: u32, id: u64, p: &'static [u8]) -> Entry {
        Entry {
            origin: NodeId(o),
            incarnation: 2,
            id: ProposalId(id),
            payload: Bytes::from_static(p),
        }
    }

    #[test]
    fn empty_batch_is_four_zero_bytes() {
        assert_eq!(encode(&[]).to_vec(), vec![0, 0, 0, 0]);
    }

    #[test]
    fn batch_round_trips() {
        let es = vec![entry(1, 7, b"put a"), entry(3, 9, b"")];
        assert_eq!(decode(&encode(&es)), Ok(es));
    }

    #[test]
    fn huge_entry_count_does_not_preallocate() {
        let buf = Bytes::from_static(&[0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(decode(&buf), Err(DecodeError::Truncated));
    }

    #[test]
    fn decode_never_panics() {
        bolero::check!().for_each(|input: &[u8]| {
            let _ = decode(&Bytes::copy_from_slice(input));
        });
    }
}

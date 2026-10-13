//! Encoding of the opaque value decided in one slot: a list of proposed entries.

use alloc::vec::Vec;
use bytes::{BufMut, Bytes};

use crate::codec::{DecodeError, Reader};
use crate::types::{NodeId, ProposalId};

const ENTRY_APP: u8 = 0;
const ENTRY_BARRIER: u8 = 1;

/// One application payload committed in the log, tagged with where it was proposed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
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

/// An entry as carried in a batch: application data, or a read barrier (empty payload)
/// that only marks a log position.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(crate) struct Item {
    pub entry: Entry,
    pub barrier: bool,
}

impl Item {
    pub fn key(&self) -> (NodeId, u64, ProposalId) {
        (self.entry.origin, self.entry.incarnation, self.entry.id)
    }
}

pub(crate) fn encoded_len(i: &Item) -> usize {
    4 + 8 + 8 + 1 + 4 + i.entry.payload.len()
}

pub(crate) fn encode(items: &[Item]) -> Bytes {
    let mut out = Vec::with_capacity(4 + items.iter().map(encoded_len).sum::<usize>());
    out.put_u32_le(items.len() as u32);
    for Item { entry: e, barrier } in items {
        out.put_u32_le(e.origin.0);
        out.put_u64_le(e.incarnation);
        out.put_u64_le(e.id.0);
        out.put_u8(if *barrier { ENTRY_BARRIER } else { ENTRY_APP });
        out.put_u32_le(e.payload.len() as u32);
        out.put_slice(&e.payload);
    }
    Bytes::from(out)
}

pub(crate) fn decode(buf: &Bytes) -> Result<Vec<Item>, DecodeError> {
    let mut r = Reader::new(buf);
    let n = r.u32()? as usize;
    // Each entry takes at least 25 bytes, which bounds the allocation by the input size.
    let mut entries = Vec::with_capacity(n.min(buf.len() / 25));
    for _ in 0..n {
        let origin = NodeId(r.u32()?);
        let incarnation = r.u64()?;
        let id = ProposalId(r.u64()?);
        let barrier = match r.u8()? {
            ENTRY_APP => false,
            ENTRY_BARRIER => true,
            _ => return Err(DecodeError::BadTag),
        };
        let len = r.u32()? as usize;
        let start = r.pos();
        r.take(len)?;
        let entry = Entry { origin, incarnation, id, payload: buf.slice(start..start + len) };
        entries.push(Item { entry, barrier });
    }
    r.finish()?;
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn entry(o: u32, id: u64, p: &'static [u8]) -> Item {
        let entry =
            Entry { origin: NodeId(o), incarnation: 2, id: ProposalId(id), payload: Bytes::from_static(p) };
        Item { entry, barrier: id % 2 == 1 }
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

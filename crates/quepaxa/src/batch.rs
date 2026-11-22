//! Encoding of the opaque value decided in one slot: a list of proposed entries, and
//! references to data batches whose entries travel apart from consensus (#172).
//!
//! Every item has the same frame: origin, incarnation, id, kind, then a length-prefixed
//! payload. An `ENTRY_REF` item names a data batch by its [`DataId`] (the frame's first three
//! fields) and carries the batch's length and CRC-32C as its 8-byte payload: 33 bytes in all.

use alloc::vec::Vec;
use bytes::{BufMut, Bytes};

use crate::codec::{DecodeError, Reader};
use crate::types::{NodeId, ProposalId};

const ENTRY_APP: u8 = 0;
const ENTRY_BARRIER: u8 = 1;
const ENTRY_RECONFIGURE: u8 = 2;
const ENTRY_REF: u8 = 3;

/// The payload of an `ENTRY_REF` item: the batch's length and CRC-32C.
const REF_PAYLOAD: usize = 8;

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

/// What an entry in a batch is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(crate) enum Kind {
    /// Application data, handed to the application in `Commit`.
    App,
    /// A barrier entry (empty payload) that only marks a log position; retirement uses it.
    Barrier,
    /// A membership change (payload: `crate::schedule::encode_members`).
    Reconfigure,
}

/// An entry as carried in a batch.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(crate) struct Item {
    pub entry: Entry,
    pub kind: Kind,
}

impl Item {
    pub fn key(&self) -> (NodeId, u64, ProposalId) {
        (self.entry.origin, self.entry.incarnation, self.entry.id)
    }
}

/// Identity of a data batch: unique per origin, incarnation and the origin's count of
/// batches in that incarnation (the shape of [`crate::proposal::ValueKey`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct DataId {
    pub origin: NodeId,
    pub incarnation: u64,
    pub seq: u64,
}

/// A reference to a data batch in a slot value. `len` and `crc` check the content, they do
/// not identify it: a batch whose bytes disagree with them is malformed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(crate) struct Ref {
    pub id: DataId,
    pub len: u32,
    pub crc: u32,
}

const HEADER: usize = 4;

fn encoded_len(i: &Item) -> usize {
    4 + 8 + 8 + 1 + 4 + i.entry.payload.len()
}

/// Whether `item` fits in a batch of at most `max` encoded bytes on its own.
pub(crate) fn fits_alone(item: &Item, max: usize) -> bool {
    HEADER + encoded_len(item) <= max
}

/// Collects items into a batch of at most `max` encoded bytes; the first item is always
/// taken (items too large on their own are rejected when proposed).
pub(crate) struct Builder {
    items: Vec<Item>,
    size: usize,
    max: usize,
}

impl Builder {
    pub fn new(max: usize) -> Self {
        Self { items: Vec::new(), size: HEADER, max }
    }

    pub fn fits(&self, item: &Item) -> bool {
        self.items.is_empty() || self.size + encoded_len(item) <= self.max
    }

    pub fn push(&mut self, item: Item) {
        self.size += encoded_len(&item);
        self.items.push(item);
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The items collected so far; the builder starts over empty.
    pub fn take(&mut self) -> Vec<Item> {
        self.size = HEADER;
        core::mem::take(&mut self.items)
    }
}

pub(crate) fn encode(items: &[Item]) -> Bytes {
    let mut out = Vec::with_capacity(HEADER + items.iter().map(encoded_len).sum::<usize>());
    out.put_u32_le(items.len() as u32);
    for Item { entry: e, kind } in items {
        out.put_u32_le(e.origin.0);
        out.put_u64_le(e.incarnation);
        out.put_u64_le(e.id.0);
        out.put_u8(match kind {
            Kind::App => ENTRY_APP,
            Kind::Barrier => ENTRY_BARRIER,
            Kind::Reconfigure => ENTRY_RECONFIGURE,
        });
        out.put_u32_le(e.payload.len() as u32);
        out.put_slice(&e.payload);
    }
    Bytes::from(out)
}

/// The membership changes in an encoded batch, in order: `(key, payload)`. Allocates only
/// when there is one, so it is cheap on every decided slot. A batch [`decode`] rejects has
/// none: the same checks apply here.
pub(crate) fn changes(buf: &Bytes) -> Vec<((NodeId, u64, ProposalId), Bytes)> {
    let mut found = Vec::new();
    let mut r = Reader::new(buf);
    let Ok(n) = r.u32() else { return found };
    for _ in 0..n {
        let (Ok(origin), Ok(incarnation), Ok(id), Ok(kind), Ok(len)) =
            (r.u32(), r.u64(), r.u64(), r.u8(), r.u32())
        else {
            return Vec::new();
        };
        let start = r.pos();
        if r.take(len as usize).is_err() {
            return Vec::new();
        }
        // A reference is refused as `decode` refuses it, until rules R and P resolve it.
        if !matches!(kind, ENTRY_APP | ENTRY_BARRIER | ENTRY_RECONFIGURE) || id == u64::MAX {
            return Vec::new();
        }
        if kind == ENTRY_RECONFIGURE {
            found.push((
                (NodeId(origin), incarnation, ProposalId(id)),
                buf.slice(start..start + len as usize),
            ));
        }
    }
    if r.finish().is_err() { Vec::new() } else { found }
}

/// The references in an encoded batch, in order. Allocates only when there is one. A batch
/// that is malformed apart from its references has none.
pub(crate) fn refs(buf: &Bytes) -> Vec<Ref> {
    let mut found = Vec::new();
    let mut r = Reader::new(buf);
    let Ok(n) = r.u32() else { return found };
    for _ in 0..n {
        let (Ok(origin), Ok(incarnation), Ok(seq), Ok(kind), Ok(len)) =
            (r.u32(), r.u64(), r.u64(), r.u8(), r.u32())
        else {
            return Vec::new();
        };
        let Ok(payload) = r.take(len as usize) else { return Vec::new() };
        if !matches!(kind, ENTRY_APP | ENTRY_BARRIER | ENTRY_RECONFIGURE | ENTRY_REF) || seq == u64::MAX {
            return Vec::new();
        }
        if kind == ENTRY_REF {
            let Some(r) = parse_ref(DataId { origin: NodeId(origin), incarnation, seq }, payload) else {
                return Vec::new();
            };
            found.push(r);
        }
    }
    if r.finish().is_err() { Vec::new() } else { found }
}

fn parse_ref(id: DataId, payload: &[u8]) -> Option<Ref> {
    if payload.len() != REF_PAYLOAD {
        return None;
    }
    let (fields, _) = payload.as_chunks::<4>();
    Some(Ref { id, len: u32::from_le_bytes(fields[0]), crc: u32::from_le_bytes(fields[1]) })
}

/// Appends `r` as an `ENTRY_REF` item to `batch`, an encoded batch.
#[cfg(test)]
pub(crate) fn push_ref(batch: &Bytes, r: Ref) -> Bytes {
    let mut out = batch.to_vec();
    let n = u32::from_le_bytes([out[0], out[1], out[2], out[3]]) + 1;
    out[..4].copy_from_slice(&n.to_le_bytes());
    out.put_u32_le(r.id.origin.0);
    out.put_u64_le(r.id.incarnation);
    out.put_u64_le(r.id.seq);
    out.put_u8(ENTRY_REF);
    out.put_u32_le(REF_PAYLOAD as u32);
    out.put_u32_le(r.len);
    out.put_u32_le(r.crc);
    Bytes::from(out)
}

/// The inline items of an encoded batch. A batch holding a well-formed reference is
/// [`DecodeError::Refused`]: nothing resolves references to their entries yet (#172).
pub(crate) fn decode(buf: &Bytes) -> Result<Vec<Item>, DecodeError> {
    let mut r = Reader::new(buf);
    let n = r.u32()? as usize;
    // Each entry takes at least 25 bytes, which bounds the allocation by the input size.
    let mut entries = Vec::with_capacity(n.min(buf.len() / 25));
    for _ in 0..n {
        let origin = NodeId(r.u32()?);
        let incarnation = r.u64()?;
        // Ids stop below u64::MAX, so a dedup window's watermark never overflows.
        let id = match r.u64()? {
            u64::MAX => return Err(DecodeError::BadTag),
            id => ProposalId(id),
        };
        let kind = match r.u8()? {
            ENTRY_APP => Kind::App,
            ENTRY_BARRIER => Kind::Barrier,
            ENTRY_RECONFIGURE => Kind::Reconfigure,
            ENTRY_REF => {
                let len = r.u32()? as usize;
                let payload = r.take(len)?;
                let id = DataId { origin, incarnation, seq: id.0 };
                return Err(parse_ref(id, payload).map_or(DecodeError::BadTag, |_| DecodeError::Refused));
            }
            _ => return Err(DecodeError::BadTag),
        };
        let len = r.u32()? as usize;
        let start = r.pos();
        r.take(len)?;
        let entry = Entry { origin, incarnation, id, payload: buf.slice(start..start + len) };
        entries.push(Item { entry, kind });
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
        Item { entry, kind: [Kind::App, Kind::Barrier, Kind::Reconfigure][id as usize % 3] }
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
    fn builder_fills_batches_exactly_up_to_the_limit() {
        let item = entry(1, 7, b"put a");
        let max = encode(&[item.clone(), item.clone()]).len();
        assert!(fits_alone(&item, max) && !fits_alone(&item, encode(core::slice::from_ref(&item)).len() - 1));
        let mut b = Builder::new(max);
        for _ in 0..2 {
            assert!(b.fits(&item));
            b.push(item.clone());
        }
        assert!(!b.fits(&item));
        assert_eq!(encode(&b.take()).len(), max);
        assert!(b.is_empty() && b.fits(&item));
    }

    #[test]
    fn builder_takes_one_oversized_item() {
        assert!(Builder::new(1).fits(&entry(1, 7, b"put a")));
    }

    #[test]
    fn changes_come_only_from_batches_that_decode() {
        let es = vec![entry(1, 2, b"\x01\0\0\0\x05\0\0\0"), entry(1, 3, b"")];
        let good = encode(&es);
        assert_eq!(changes(&good).len(), 1);
        // The same batch with an unknown kind in its last entry is corrupt as a whole.
        let mut bad = good.to_vec();
        let at = bad.len() - 4 - 1;
        bad[at] = 9;
        let bad = Bytes::from(bad);
        assert!(decode(&bad).is_err());
        assert!(changes(&bad).is_empty());
    }

    #[test]
    fn the_largest_id_is_rejected() {
        let ok = encode(&[entry(1, u64::MAX - 1, b"\x01\0\0\0\x05\0\0\0")]);
        assert!(decode(&ok).is_ok());
        assert_eq!(changes(&ok).len(), 1);
        let mut bad = ok.to_vec();
        // The id follows the entry count (4 bytes), origin (4) and incarnation (8).
        bad[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        let bad = Bytes::from(bad);
        assert!(decode(&bad).is_err());
        assert!(changes(&bad).is_empty());
    }

    #[test]
    fn huge_entry_count_does_not_preallocate() {
        let buf = Bytes::from_static(&[0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(decode(&buf), Err(DecodeError::Truncated));
    }

    fn reference() -> Ref {
        Ref { id: DataId { origin: NodeId(3), incarnation: 4, seq: 5 }, len: 0x0102, crc: 0xA1B2_C3D4 }
    }

    #[test]
    fn a_reference_matches_golden_bytes_in_33_bytes() {
        let bytes = push_ref(&encode(&[]), reference());
        let golden = "01000000 03000000 0400000000000000 0500000000000000 03 08000000 02010000 d4c3b2a1";
        let golden: Vec<u8> = golden
            .split(' ')
            .flat_map(|w| (0..w.len()).step_by(2).map(move |i| u8::from_str_radix(&w[i..i + 2], 16).unwrap()))
            .collect();
        assert_eq!(bytes.to_vec(), golden);
        assert_eq!(bytes.len() - HEADER, 33);
    }

    #[test]
    fn references_are_found_and_refused() {
        let es = vec![entry(1, 2, b"\x01\0\0\0\x05\0\0\0"), entry(1, 3, b"")];
        let inline = encode(&es);
        assert!(refs(&inline).is_empty());
        let mixed = push_ref(&inline, reference());
        assert_eq!(refs(&mixed), vec![reference()]);
        assert_eq!(decode(&mixed), Err(DecodeError::Refused));
        // The change in the same batch is refused with it, as `decode` refuses its entries.
        assert!(changes(&mixed).is_empty());
    }

    #[test]
    fn a_reference_with_a_wrong_payload_is_corrupt() {
        let mut bad = push_ref(&encode(&[]), reference()).to_vec();
        // Its payload length (after count 4, id 20, kind 1) says 7, and one byte is dropped.
        bad[25..29].copy_from_slice(&7u32.to_le_bytes());
        bad.pop();
        let bad = Bytes::from(bad);
        assert_eq!(decode(&bad), Err(DecodeError::BadTag));
        assert!(refs(&bad).is_empty());
        // Trailing bytes after a good reference make the whole batch corrupt.
        let mut long = push_ref(&encode(&[]), reference()).to_vec();
        long.push(0);
        assert!(refs(&Bytes::from(long)).is_empty());
    }

    #[test]
    fn decode_never_panics() {
        bolero::check!().for_each(|input: &[u8]| {
            let buf = Bytes::copy_from_slice(input);
            let decoded = decode(&buf);
            let found = refs(&buf);
            // Only a well-formed batch yields references, and `decode` refuses it.
            assert!(found.is_empty() || decoded == Err(DecodeError::Refused));
            if decoded.is_ok() {
                assert!(found.is_empty());
            }
            let _ = changes(&buf);
        });
    }
}

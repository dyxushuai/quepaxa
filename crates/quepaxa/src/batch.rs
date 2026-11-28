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

/// High bit of the batch item count header: set when a follower proposed the batch while
/// hedging around a live leader so `Schedule::observe` leaves the miss streak unchanged (#258).
/// Valid item counts stay far below 2^31 because each encoded item is at least 25 bytes.
const HEDGE_FLAG: u32 = 1 << 31;

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

/// One element of a slot value or forward: an inline entry, or a reference to a data batch.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(crate) enum Piece {
    Entry(Item),
    Ref(Ref),
}

const HEADER: usize = 4;
const REF_ENCODED_LEN: usize = 4 + 8 + 8 + 1 + 4 + REF_PAYLOAD;

fn encoded_len(i: &Item) -> usize {
    4 + 8 + 8 + 1 + 4 + i.entry.payload.len()
}

pub(crate) fn piece_weight(p: &Piece) -> usize {
    match p {
        Piece::Entry(i) => encoded_len(i),
        // Count both the 33-byte wire ref and the referenced batch's encoded length so the
        // expanded slot stays within `max_batch_bytes`.
        Piece::Ref(r) => REF_ENCODED_LEN.saturating_add(r.len as usize),
    }
}

/// Whether `item` fits in a batch of at most `max` encoded bytes on its own.
pub(crate) fn fits_alone(item: &Item, max: usize) -> bool {
    HEADER + encoded_len(item) <= max
}

/// Collects inline items into a batch of at most `max` encoded bytes; the first item is
/// always taken (items too large on their own are rejected when proposed).
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

/// Collects slot/forward pieces (inline entries and references) within `max` bytes of
/// expanded batch budget; the first piece is always taken.
pub(crate) struct PieceBuilder {
    pieces: Vec<Piece>,
    size: usize,
    max: usize,
}

impl PieceBuilder {
    pub fn new(max: usize) -> Self {
        Self { pieces: Vec::new(), size: HEADER, max }
    }

    pub fn fits(&self, piece: &Piece) -> bool {
        self.pieces.is_empty() || self.size.saturating_add(piece_weight(piece)) <= self.max
    }

    pub fn push(&mut self, piece: Piece) {
        self.size = self.size.saturating_add(piece_weight(&piece));
        self.pieces.push(piece);
    }

    pub fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn take(&mut self) -> Vec<Piece> {
        self.size = HEADER;
        core::mem::take(&mut self.pieces)
    }
}

fn put_item(out: &mut Vec<u8>, Item { entry: e, kind }: &Item) {
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

fn put_ref(out: &mut Vec<u8>, r: Ref) {
    out.put_u32_le(r.id.origin.0);
    out.put_u64_le(r.id.incarnation);
    out.put_u64_le(r.id.seq);
    out.put_u8(ENTRY_REF);
    out.put_u32_le(REF_PAYLOAD as u32);
    out.put_u32_le(r.len);
    out.put_u32_le(r.crc);
}

pub(crate) fn encode(items: &[Item]) -> Bytes {
    let mut out = Vec::with_capacity(HEADER + items.iter().map(encoded_len).sum::<usize>());
    out.put_u32_le(items.len() as u32);
    for item in items {
        put_item(&mut out, item);
    }
    Bytes::from(out)
}

pub(crate) fn encode_pieces(pieces: &[Piece]) -> Bytes {
    encode_pieces_flagged(pieces, false)
}

pub(crate) fn encode_pieces_flagged(pieces: &[Piece], hedge: bool) -> Bytes {
    debug_assert!(pieces.len() < HEDGE_FLAG as usize);
    let wire_len: usize = pieces
        .iter()
        .map(|p| match p {
            Piece::Entry(i) => encoded_len(i),
            Piece::Ref(_) => REF_ENCODED_LEN,
        })
        .sum();
    let mut out = Vec::with_capacity(HEADER + wire_len);
    let count = (pieces.len() as u32) | if hedge { HEDGE_FLAG } else { 0 };
    out.put_u32_le(count);
    for piece in pieces {
        match piece {
            Piece::Entry(i) => put_item(&mut out, i),
            Piece::Ref(r) => put_ref(&mut out, *r),
        }
    }
    Bytes::from(out)
}

fn read_count(r: &mut Reader<'_>) -> Result<usize, DecodeError> {
    Ok((r.u32()? & !HEDGE_FLAG) as usize)
}

/// Whether an encoded slot value was proposed while hedging around a talking leader.
/// Returns `false` for non-hedged and malformed batches.
pub(crate) fn is_hedge(buf: &Bytes) -> bool {
    if buf.len() < HEADER {
        return false;
    }
    let header = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    (header & HEDGE_FLAG) != 0 && decode_pieces(buf).is_ok()
}

/// The membership changes in an encoded batch, in order: `(key, payload)`. Allocates only
/// when there is one, so it is cheap on every decided slot. Control entries are always
/// inline, so well-formed references are skipped; a batch [`decode_pieces`] rejects has none.
pub(crate) fn changes(buf: &Bytes) -> Vec<((NodeId, u64, ProposalId), Bytes)> {
    let mut found = Vec::new();
    let mut r = Reader::new(buf);
    let Ok(n) = read_count(&mut r) else { return found };
    for _ in 0..n {
        let (Ok(origin), Ok(incarnation), Ok(id), Ok(kind), Ok(len)) =
            (r.u32(), r.u64(), r.u64(), r.u8(), r.u32())
        else {
            return Vec::new();
        };
        let start = r.pos();
        let Ok(payload) = r.take(len as usize) else {
            return Vec::new();
        };
        if id == u64::MAX {
            return Vec::new();
        }
        match kind {
            ENTRY_APP | ENTRY_BARRIER => {}
            ENTRY_RECONFIGURE => {
                found.push((
                    (NodeId(origin), incarnation, ProposalId(id)),
                    buf.slice(start..start + len as usize),
                ));
            }
            ENTRY_REF => {
                if parse_ref(DataId { origin: NodeId(origin), incarnation, seq: id }, payload).is_none() {
                    return Vec::new();
                }
            }
            _ => return Vec::new(),
        }
    }
    if r.finish().is_err() { Vec::new() } else { found }
}

/// The references in an encoded batch, in order. Allocates only when there is one. A batch
/// that is malformed apart from its references has none.
pub(crate) fn refs(buf: &Bytes) -> Vec<Ref> {
    let mut found = Vec::new();
    let mut r = Reader::new(buf);
    let Ok(n) = read_count(&mut r) else { return found };
    for _ in 0..n {
        let Ok(hdr) = r.take(25) else { return Vec::new() };
        let origin = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        let incarnation =
            u64::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7], hdr[8], hdr[9], hdr[10], hdr[11]]);
        let seq =
            u64::from_le_bytes([hdr[12], hdr[13], hdr[14], hdr[15], hdr[16], hdr[17], hdr[18], hdr[19]]);
        let kind = hdr[20];
        let len = u32::from_le_bytes([hdr[21], hdr[22], hdr[23], hdr[24]]);
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
    let header = u32::from_le_bytes([out[0], out[1], out[2], out[3]]);
    let updated = (header & HEDGE_FLAG) | ((header & !HEDGE_FLAG) + 1);
    out[..4].copy_from_slice(&updated.to_le_bytes());
    out.put_u32_le(r.id.origin.0);
    out.put_u64_le(r.id.incarnation);
    out.put_u64_le(r.id.seq);
    out.put_u8(ENTRY_REF);
    out.put_u32_le(REF_PAYLOAD as u32);
    out.put_u32_le(r.len);
    out.put_u32_le(r.crc);
    Bytes::from(out)
}

/// The pieces (inline items and data-batch references) of an encoded slot value or forward.
pub(crate) fn decode_pieces(buf: &Bytes) -> Result<Vec<Piece>, DecodeError> {
    let mut r = Reader::new(buf);
    let n = read_count(&mut r)?;
    // Each entry takes at least 25 bytes, which bounds the allocation by the input size.
    let mut pieces = Vec::with_capacity(n.min(buf.len() / 25));
    for _ in 0..n {
        let origin = NodeId(r.u32()?);
        let incarnation = r.u64()?;
        // Ids stop below u64::MAX, so a dedup window's watermark never overflows.
        let id = match r.u64()? {
            u64::MAX => return Err(DecodeError::BadTag),
            id => ProposalId(id),
        };
        let tag = r.u8()?;
        let len = r.u32()? as usize;
        let start = r.pos();
        let payload = r.take(len)?;
        match tag {
            ENTRY_APP | ENTRY_BARRIER | ENTRY_RECONFIGURE => {
                let kind = match tag {
                    ENTRY_APP => Kind::App,
                    ENTRY_BARRIER => Kind::Barrier,
                    _ => Kind::Reconfigure,
                };
                let entry = Entry { origin, incarnation, id, payload: buf.slice(start..start + len) };
                pieces.push(Piece::Entry(Item { entry, kind }));
            }
            ENTRY_REF => {
                let id = DataId { origin, incarnation, seq: id.0 };
                let reference = parse_ref(id, payload).ok_or(DecodeError::BadTag)?;
                pieces.push(Piece::Ref(reference));
            }
            _ => return Err(DecodeError::BadTag),
        }
    }
    r.finish()?;
    Ok(pieces)
}

/// Decodes an encoded batch directly into inline items, expanding any `ENTRY_REF` via `expand`.
/// Allocates a single `Vec<Item>` (no intermediate `Vec<Piece>` on inline batches).
pub(crate) fn decode_expanded(
    buf: &Bytes,
    mut expand: impl FnMut(Ref) -> Result<Vec<Item>, DecodeError>,
) -> Result<Vec<Item>, DecodeError> {
    let mut r = Reader::new(buf);
    let n = read_count(&mut r)?;
    let mut items = Vec::with_capacity(n.min(buf.len() / 25));
    for _ in 0..n {
        let origin = NodeId(r.u32()?);
        let incarnation = r.u64()?;
        let id = match r.u64()? {
            u64::MAX => return Err(DecodeError::BadTag),
            id => ProposalId(id),
        };
        let tag = r.u8()?;
        let len = r.u32()? as usize;
        let start = r.pos();
        let payload = r.take(len)?;
        match tag {
            ENTRY_APP | ENTRY_BARRIER | ENTRY_RECONFIGURE => {
                let kind = match tag {
                    ENTRY_APP => Kind::App,
                    ENTRY_BARRIER => Kind::Barrier,
                    _ => Kind::Reconfigure,
                };
                let entry = Entry { origin, incarnation, id, payload: buf.slice(start..start + len) };
                items.push(Item { entry, kind });
            }
            ENTRY_REF => {
                let id = DataId { origin, incarnation, seq: id.0 };
                let reference = parse_ref(id, payload).ok_or(DecodeError::BadTag)?;
                items.extend(expand(reference)?);
            }
            _ => return Err(DecodeError::BadTag),
        }
    }
    r.finish()?;
    Ok(items)
}

/// Decodes a data batch's content: application entries only (no control entries and no
/// nested references).
pub(crate) fn decode_content(buf: &Bytes) -> Result<Vec<Item>, DecodeError> {
    let mut r = Reader::new(buf);
    let n = r.u32()? as usize;
    let mut items = Vec::with_capacity(n.min(buf.len() / 25));
    for _ in 0..n {
        let origin = NodeId(r.u32()?);
        let incarnation = r.u64()?;
        let id = match r.u64()? {
            u64::MAX => return Err(DecodeError::BadTag),
            id => ProposalId(id),
        };
        let tag = r.u8()?;
        if tag != ENTRY_APP {
            return Err(DecodeError::BadTag);
        }
        let len = r.u32()? as usize;
        let start = r.pos();
        let _ = r.take(len)?;
        let entry = Entry { origin, incarnation, id, payload: buf.slice(start..start + len) };
        items.push(Item { entry, kind: Kind::App });
    }
    r.finish()?;
    Ok(items)
}

/// The inline items of an encoded batch (no references).
#[cfg(test)]
pub(crate) fn decode(buf: &Bytes) -> Result<Vec<Item>, DecodeError> {
    let pieces = decode_pieces(buf)?;
    let mut items = Vec::with_capacity(pieces.len());
    for piece in pieces {
        match piece {
            Piece::Entry(item) => items.push(item),
            Piece::Ref(_) => return Err(DecodeError::BadTag),
        }
    }
    Ok(items)
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
    fn references_and_inline_items_round_trip_together() {
        let es = vec![entry(1, 2, b"\x01\0\0\0\x05\0\0\0"), entry(1, 3, b"")];
        let inline = encode(&es);
        assert!(refs(&inline).is_empty());
        let mixed = push_ref(&inline, reference());
        assert_eq!(refs(&mixed), vec![reference()]);
        let pieces = vec![Piece::Entry(es[0].clone()), Piece::Entry(es[1].clone()), Piece::Ref(reference())];
        assert_eq!(encode_pieces(&pieces), mixed);
        assert_eq!(decode_pieces(&mixed), Ok(pieces));
        // Control entries in the same batch remain visible without resolving references.
        assert_eq!(changes(&mixed).len(), 1);
        // Data batch content allows only application entries.
        let app = vec![Item { kind: Kind::App, ..es[0].clone() }];
        assert_eq!(decode_content(&encode(&app)), Ok(app));
        assert_eq!(decode_content(&mixed), Err(DecodeError::BadTag));
        assert_eq!(decode_content(&inline), Err(DecodeError::BadTag));
    }

    #[test]
    fn a_reference_with_a_wrong_payload_is_corrupt() {
        let mut bad = push_ref(&encode(&[]), reference()).to_vec();
        // Its payload length (after count 4, id 20, kind 1) says 7, and one byte is dropped.
        bad[25..29].copy_from_slice(&7u32.to_le_bytes());
        bad.pop();
        let bad = Bytes::from(bad);
        assert_eq!(decode_pieces(&bad), Err(DecodeError::BadTag));
        assert!(refs(&bad).is_empty());
        // Trailing bytes after a good reference make the whole batch corrupt.
        let mut long = push_ref(&encode(&[]), reference()).to_vec();
        long.push(0);
        assert!(refs(&Bytes::from(long)).is_empty());
    }

    #[test]
    fn hedged_batches_round_trip_with_zero_overhead_and_flag_checks_framing() {
        let pieces = vec![
            Piece::Entry(entry(1, 2, b"\x01\0\0\0\x05\0\0\0")),
            Piece::Entry(entry(1, 3, b"put a")),
            Piece::Ref(reference()),
        ];
        let plain = encode_pieces(&pieces);
        let hedged = encode_pieces_flagged(&pieces, true);
        assert_eq!(hedged.len(), plain.len(), "hedge flag adds no bytes");
        assert!(!is_hedge(&plain));
        assert!(is_hedge(&hedged));
        assert_eq!(decode_pieces(&hedged), Ok(pieces.clone()));
        assert_eq!(refs(&hedged), vec![reference()]);
        assert_eq!(changes(&hedged).len(), 1);
        assert_eq!(decode_expanded(&hedged, |_| Ok(vec![entry(2, 4, b"data")])).map(|v| v.len()), Ok(3));
        // Data batch content rejects the hedge flag.
        let app = vec![Piece::Entry(Item { kind: Kind::App, ..entry(1, 1, b"a") })];
        assert!(decode_content(&encode_pieces_flagged(&app, true)).is_err());
        // A corrupt batch with bit 31 set is not treated as a hedge.
        let mut corrupt = hedged.to_vec();
        corrupt.push(0);
        assert!(!is_hedge(&Bytes::from(corrupt)));
        assert!(!is_hedge(&Bytes::from_static(&[0xFF, 0xFF, 0xFF, 0xFF])));
    }

    #[test]
    fn decode_never_panics() {
        bolero::check!().for_each(|input: &[u8]| {
            let buf = Bytes::copy_from_slice(input);
            let decoded = decode_pieces(&buf);
            let found = refs(&buf);
            if let Ok(pieces) = &decoded {
                let expected: Vec<Ref> = pieces
                    .iter()
                    .filter_map(|p| match p {
                        Piece::Ref(r) => Some(*r),
                        Piece::Entry(_) => None,
                    })
                    .collect();
                assert_eq!(found, expected);
            } else {
                assert!(found.is_empty());
                assert!(!is_hedge(&buf));
            }
            let _ = decode_content(&buf);
            let _ = changes(&buf);
        });
    }
}

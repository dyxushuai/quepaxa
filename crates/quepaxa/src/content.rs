//! The data store (#172, `docs/design/batch-ids.md`): the content of data batches that slot
//! values reference, kept while something needs it and evicted otherwise. It mirrors
//! `verification/tla/Content.tla`: `Pinned` is [`Held::pinned`], `Evict` the cap below and
//! [`Store::raise_floor`], and a crash keeps only what replay [`Store::restore`]s.
//!
//! A batch is pinned while a register, a decision or a working value of a slot at or above
//! the floor references it, until compaction passes that slot (DEV-4: no reference counts),
//! and while it is kept (an origin's own batch until its entries commit, a queued
//! reference). Unpinned batches are capped in bytes per origin, the oldest going first.
//! Eviction is always safe: rules R and P never rely on unpinned content, and a batch
//! evicted and needed again is fetched.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use bytes::Bytes;

use crate::batch::{DataId, Ref};
use crate::crc::crc32c;
use crate::types::{NodeId, Slot};

#[derive(Debug)]
#[cfg_attr(feature = "model", derive(Clone, Hash))]
struct Held {
    bytes: Bytes,
    crc: u32,
    /// The highest slot whose register, decision or working value references the batch.
    slot: Option<Slot>,
    /// Kept whatever the slots.
    kept: bool,
}

impl Held {
    fn pinned(&self, floor: u64) -> bool {
        self.kept || self.slot.is_some_and(|s| s.0 >= floor)
    }
}

/// Why a reference cannot be pinned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PinError {
    /// The store does not hold the batch.
    Missing,
    /// The store holds other bytes under that id: a length or CRC-32C that disagrees with
    /// the reference (an identity reused, for example restarted on an empty disk).
    Mismatch,
}

#[derive(Debug)]
#[cfg_attr(feature = "model", derive(Clone, Hash))]
pub(crate) struct Store {
    batches: BTreeMap<DataId, Held>,
    /// Bytes of unpinned batches per origin; at most `cap` each after every change.
    loose: BTreeMap<NodeId, usize>,
    cap: usize,
    /// First slot not covered by a snapshot.
    floor: u64,
}

impl Store {
    /// A store keeping at most `cap` bytes of unpinned batches per origin.
    pub fn new(cap: usize) -> Self {
        Self { batches: BTreeMap::new(), loose: BTreeMap::new(), cap, floor: 0 }
    }

    /// The content of batch `id`, if held.
    #[cfg_attr(not(test), expect(dead_code, reason = "rules R and P read content (#172)"))]
    pub fn get(&self, id: &DataId) -> Option<&Bytes> {
        self.batches.get(id).map(|h| &h.bytes)
    }

    /// Adds a received batch, unpinned; older unpinned batches of its origin (or this one,
    /// if it is the oldest) are evicted beyond the cap. Whether the store holds it after.
    #[cfg_attr(not(test), expect(dead_code, reason = "`Data` is stored with rules R and P (#172)"))]
    pub fn insert(&mut self, id: DataId, bytes: Bytes) -> bool {
        if !self.batches.contains_key(&id) {
            self.restore(id, bytes);
            self.trim(id.origin);
        }
        self.batches.contains_key(&id)
    }

    /// Adds a batch from a durable `Data` record, unpinned and without evicting anything:
    /// records later in the replay may pin it. [`Store::raise_floor`] applies the cap after.
    pub fn restore(&mut self, id: DataId, bytes: Bytes) {
        if let Some(old) = self.batches.get(&id) {
            if old.pinned(self.floor) {
                return;
            }
            unloose(&mut self.loose, id.origin, old.bytes.len());
        }
        *self.loose.entry(id.origin).or_default() += bytes.len();
        let held = Held { crc: crc32c(&bytes), bytes, slot: None, kept: false };
        self.batches.insert(id, held);
    }

    /// Pins `r`'s batch for `slot`, after checking its length and CRC-32C. A slot below the
    /// floor pins nothing.
    pub fn pin(&mut self, r: Ref, slot: Slot) -> Result<(), PinError> {
        let floor = self.floor;
        let held = self.batches.get_mut(&r.id).ok_or(PinError::Missing)?;
        if held.bytes.len() != r.len as usize || held.crc != r.crc {
            return Err(PinError::Mismatch);
        }
        if slot.0 < floor {
            return Ok(());
        }
        let was = held.pinned(floor);
        held.slot = held.slot.max(Some(slot));
        if !was {
            let len = held.bytes.len();
            unloose(&mut self.loose, r.id.origin, len);
        }
        Ok(())
    }

    /// Pins, for `slot`, every reference in `value` (an encoded batch) whose batch is held.
    /// The error, if any, is the worst seen: a mismatch, else a batch not held.
    pub fn pin_refs(&mut self, value: &Bytes, slot: Slot) -> Result<(), PinError> {
        let mut worst = Ok(());
        for r in crate::batch::refs(value) {
            match self.pin(r, slot) {
                Err(PinError::Mismatch) => worst = Err(PinError::Mismatch),
                Err(PinError::Missing) if worst.is_ok() => worst = Err(PinError::Missing),
                _ => {}
            }
        }
        worst
    }

    /// Keeps batch `id` whatever the slots, until [`Store::release`]. Whether it is held.
    #[cfg_attr(not(test), expect(dead_code, reason = "origins and queues keep batches with rule R (#172)"))]
    pub fn keep(&mut self, id: DataId) -> bool {
        let floor = self.floor;
        let Some(held) = self.batches.get_mut(&id) else { return false };
        if !held.pinned(floor) {
            let len = held.bytes.len();
            unloose(&mut self.loose, id.origin, len);
        }
        held.kept = true;
        true
    }

    /// Ends [`Store::keep`]: the batch stays while a slot pins it, and is loose otherwise.
    #[cfg_attr(not(test), expect(dead_code, reason = "origins and queues keep batches with rule R (#172)"))]
    pub fn release(&mut self, id: DataId) {
        let floor = self.floor;
        let Some(held) = self.batches.get_mut(&id).filter(|h| h.kept) else { return };
        held.kept = false;
        if !held.pinned(floor) {
            *self.loose.entry(id.origin).or_default() += held.bytes.len();
            self.trim(id.origin);
        }
    }

    /// Compaction to `floor`: batches pinned only by slots below it go with those slots
    /// (the snapshot covers them), and the cap applies to what is left unpinned.
    pub fn raise_floor(&mut self, floor: u64) {
        self.floor = self.floor.max(floor);
        let floor = self.floor;
        self.batches.retain(|_, h| {
            if h.slot.is_none_or(|s| s.0 >= floor) {
                return true;
            }
            // Pinned, so not loose: kept, it stays as such; otherwise it goes.
            h.slot = None;
            h.kept
        });
        let origins: Vec<NodeId> = self.loose.keys().copied().collect();
        for origin in origins {
            self.trim(origin);
        }
    }

    /// The batches a register or decided slot at or above the floor may reference, which a
    /// checkpoint rewrites ahead of the records that reference them.
    pub fn referenced(&self) -> impl Iterator<Item = (DataId, &Bytes)> {
        self.batches
            .iter()
            .filter(|(_, h)| h.slot.is_some_and(|s| s.0 >= self.floor))
            .map(|(id, h)| (*id, &h.bytes))
    }

    /// Evicts `origin`'s oldest unpinned batches until they fit the cap.
    fn trim(&mut self, origin: NodeId) {
        let Some(&total) = self.loose.get(&origin) else { return };
        if total <= self.cap {
            return;
        }
        let floor = self.floor;
        let lo = DataId { origin, incarnation: 0, seq: 0 };
        let hi = DataId { origin, incarnation: u64::MAX, seq: u64::MAX };
        let mut excess = total - self.cap;
        let mut gone = Vec::new();
        for (id, h) in self.batches.range(lo..=hi) {
            if excess == 0 {
                break;
            }
            if !h.pinned(floor) {
                excess = excess.saturating_sub(h.bytes.len());
                gone.push((*id, h.bytes.len()));
            }
        }
        for (id, len) in gone {
            self.batches.remove(&id);
            unloose(&mut self.loose, origin, len);
        }
    }
}

fn unloose(loose: &mut BTreeMap<NodeId, usize>, origin: NodeId, len: usize) {
    if let Some(n) = loose.get_mut(&origin) {
        *n -= len;
        if *n == 0 {
            loose.remove(&origin);
        }
    }
}

#[cfg(test)]
#[path = "content_tests.rs"]
mod tests;

//! The key-value state, split into shards a snapshot shares instead of copying.
//!
//! [`Kv::snapshot`] copies only the shard pointers, so a snapshot can be taken on the event
//! loop and encoded elsewhere. A write to a shard the snapshot still holds goes to that
//! shard's small overlay of pending writes instead of copying the shard; the first write
//! after the snapshot lets go folds the overlay back in. A snapshot therefore costs the
//! event loop work in proportion to the writes made while it is encoded, not to the state.
//!
//! Keys and values are bytes. Like etcd's store without history, the state carries a store
//! revision, raised once per write request, and each key its create revision, mod revision
//! and version.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::collections::btree_map;
use std::ops::Bound;
use std::sync::Arc;

const SHARDS: usize = 256;

/// A key's value and its revisions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub value: Arc<[u8]>,
    /// Revision of the write that created the key (since it last did not exist).
    pub create: u64,
    /// Revision of the key's last write.
    pub modified: u64,
    /// Writes to the key since it was created, starting at 1.
    pub version: u64,
}

type Key = Arc<[u8]>;

#[derive(Clone, Debug, Default)]
struct Shard {
    /// Written in place unless a snapshot shares it.
    base: Arc<BTreeMap<Key, Entry>>,
    /// Writes made while `base` was shared, `None` for a deletion. Empty whenever `base` is
    /// written in place: it is folded in first.
    overlay: BTreeMap<Key, Option<Entry>>,
}

/// The entry `revision` writes over `prev`.
fn written(prev: Option<&Entry>, value: Arc<[u8]>, revision: u64) -> Entry {
    let (create, version) = prev.map_or((revision, 1), |p| (p.create, p.version + 1));
    Entry { value, create, modified: revision, version }
}

impl Shard {
    /// `base` to write in place, the overlay folded into it; `None` while it is shared.
    fn owned(&mut self) -> Option<&mut BTreeMap<Key, Entry>> {
        let base = Arc::get_mut(&mut self.base)?;
        for (k, e) in std::mem::take(&mut self.overlay) {
            match e {
                Some(e) => base.insert(k, e),
                None => base.remove(&k),
            };
        }
        Some(base)
    }

    fn get_key_value(&self, key: &[u8]) -> Option<(&Key, &Entry)> {
        match self.overlay.get_key_value(key) {
            Some((k, e)) => e.as_ref().map(|e| (k, e)),
            None => self.base.get_key_value(key),
        }
    }

    fn put(&mut self, key: &[u8], value: &[u8], revision: u64) -> Option<Entry> {
        let value = Arc::from(value);
        if let Some(base) = self.owned() {
            return match base.entry(key.into()) {
                btree_map::Entry::Occupied(mut o) => {
                    let next = written(Some(o.get()), value, revision);
                    Some(o.insert(next))
                }
                btree_map::Entry::Vacant(v) => {
                    v.insert(written(None, value, revision));
                    None
                }
            };
        }
        match self.overlay.entry(key.into()) {
            btree_map::Entry::Occupied(mut o) => {
                let next = written(o.get().as_ref(), value, revision);
                o.insert(Some(next))
            }
            btree_map::Entry::Vacant(v) => {
                let prev = self.base.get(key).cloned();
                v.insert(Some(written(prev.as_ref(), value, revision)));
                prev
            }
        }
    }

    fn delete(&mut self, key: &[u8]) -> Option<Entry> {
        if let Some(base) = self.owned() {
            return base.remove(key);
        }
        if let Some(e) = self.overlay.get_mut(key) {
            return e.take();
        }
        let prev = self.base.get(key).cloned();
        if prev.is_some() {
            self.overlay.insert(key.into(), None);
        }
        prev
    }

    /// The entries from `from` on, in key order.
    fn range_from<'a>(&'a self, from: Bound<&'a [u8]>) -> impl Iterator<Item = (&'a Key, &'a Entry)> {
        merged(
            self.base.range::<[u8], _>((from, Bound::Unbounded)),
            self.overlay.range::<[u8], _>((from, Bound::Unbounded)),
        )
    }

    fn len(&self) -> usize {
        if self.overlay.is_empty() { self.base.len() } else { self.range_from(Bound::Unbounded).count() }
    }
}

/// `base` with `overlay` written over it, both in key order.
fn merged<'a>(
    base: impl Iterator<Item = (&'a Key, &'a Entry)>,
    overlay: impl Iterator<Item = (&'a Key, &'a Option<Entry>)>,
) -> impl Iterator<Item = (&'a Key, &'a Entry)> {
    let (mut base, mut overlay) = (base.peekable(), overlay.peekable());
    std::iter::from_fn(move || {
        loop {
            let from_overlay = match (base.peek(), overlay.peek()) {
                (_, None) => return base.next(),
                (None, Some(_)) => true,
                (Some((b, _)), Some((o, _))) => match o.cmp(b) {
                    Ordering::Less => true,
                    Ordering::Equal => {
                        base.next();
                        true
                    }
                    Ordering::Greater => false,
                },
            };
            if !from_overlay {
                return base.next();
            }
            if let Some((k, Some(e))) = overlay.next() {
                return Some((k, e));
            }
        }
    })
}

#[derive(Clone, Debug)]
pub struct Kv {
    shards: Vec<Shard>,
    revision: u64,
}

impl Default for Kv {
    fn default() -> Self {
        Self { shards: (0..SHARDS).map(|_| Shard::default()).collect(), revision: 0 }
    }
}

/// Equal contents, however the writes are split between shards' bases and overlays.
impl PartialEq for Kv {
    fn eq(&self, other: &Self) -> bool {
        self.revision == other.revision && self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl Eq for Kv {}

/// FNV-1a: stable across runs and platforms, so shard placement never depends on a seed.
fn shard(key: &[u8]) -> usize {
    let h =
        key.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3));
    (h % SHARDS as u64) as usize
}

/// The keys a range covers, etcd's way: an empty `end` is `key` alone, `[0]` every key from
/// `key` on, anything else `[key, end)`.
pub fn in_range(key: &[u8], end: &[u8], k: &[u8]) -> bool {
    match end {
        [] => k == key,
        [0] => k >= key,
        _ => k >= key && k < end,
    }
}

impl Kv {
    /// The store revision: the number of write requests applied.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Starts the next write request's revision.
    pub fn next_revision(&mut self) -> u64 {
        self.revision += 1;
        self.revision
    }

    /// A copy sharing every shard: pending overlays are folded first where no earlier
    /// snapshot still holds the shard, so only shard pointers are copied.
    pub fn snapshot(&mut self) -> Self {
        for s in &mut self.shards {
            s.owned();
        }
        self.clone()
    }

    pub fn get(&self, key: &[u8]) -> Option<&Entry> {
        self.shards[shard(key)].get_key_value(key).map(|(_, e)| e)
    }

    /// Writes `value` at `revision`; returns the previous entry.
    pub fn put(&mut self, key: &[u8], value: &[u8], revision: u64) -> Option<Entry> {
        self.shards[shard(key)].put(key, value, revision)
    }

    /// Removes `key`; returns its entry.
    pub fn delete(&mut self, key: &[u8]) -> Option<Entry> {
        self.shards[shard(key)].delete(key)
    }

    /// The entries in `[key, end)` (see [`in_range`]) in key order. A single key is a
    /// lookup; a span merges the shards.
    pub fn range(&self, key: &[u8], end: &[u8]) -> Vec<(Arc<[u8]>, Entry)> {
        if end.is_empty() {
            let found = self.shards[shard(key)].get_key_value(key);
            return found.map(|(k, e)| (k.clone(), e.clone())).into_iter().collect();
        }
        let mut out: Vec<(Arc<[u8]>, Entry)> = self
            .shards
            .iter()
            .flat_map(|s| s.range_from(Bound::Included(key)).take_while(|(k, _)| in_range(key, end, k)))
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect();
        out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn len(&self) -> usize {
        self.shards.iter().map(Shard::len).sum()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &Entry)> {
        self.shards.iter().flat_map(|s| s.range_from(Bound::Unbounded)).map(|(k, e)| (&**k, e))
    }

    /// A state restored from a snapshot: its revision and entries.
    pub fn restore(revision: u64, entries: impl IntoIterator<Item = (Vec<u8>, Entry)>) -> Self {
        let mut kv = Self { revision, ..Self::default() };
        for (k, e) in entries {
            Arc::make_mut(&mut kv.shards[shard(&k)].base).insert(k.into(), e);
        }
        kv
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_is_stable_and_writes_copy_no_shard() {
        let mut kv = Kv::default();
        for i in 0..1000 {
            let rev = kv.next_revision();
            kv.put(format!("k{i}").as_bytes(), b"a", rev);
        }
        let bases = |kv: &Kv| kv.shards.iter().map(|s| Arc::as_ptr(&s.base)).collect::<Vec<_>>();
        let before = bases(&kv);
        let snapshot = kv.snapshot();
        let rev = kv.next_revision();
        kv.put(b"k7", b"b", rev);
        kv.put(b"new", b"c", rev);
        kv.delete(b"k8");
        assert_eq!(
            (&*snapshot.get(b"k7").unwrap().value, &*kv.get(b"k7").unwrap().value),
            (&b"a"[..], &b"b"[..])
        );
        assert_eq!((snapshot.get(b"k8").is_some(), kv.get(b"k8")), (true, None));
        assert_eq!((kv.len(), snapshot.len()), (1000, 1000));
        assert_eq!(bases(&kv), before, "a write while the snapshot is held copies no shard");
        drop(snapshot);
        let rev = kv.next_revision();
        kv.put(b"k7", b"d", rev);
        assert_eq!(bases(&kv), before, "the overlay is folded in place");
        assert!(kv.shards[shard(b"k7")].overlay.is_empty());
        assert_eq!((kv.len(), &*kv.get(b"k7").unwrap().value, kv.get(b"k8")), (1000, &b"d"[..], None));
    }

    #[test]
    fn puts_keep_create_revision_and_count_versions() {
        let mut kv = Kv::default();
        assert_eq!(kv.put(b"a", b"1", 5), None);
        let prev = kv.put(b"a", b"2", 9).unwrap();
        assert_eq!((prev.create, prev.modified, prev.version), (5, 5, 1));
        let e = kv.get(b"a").unwrap();
        assert_eq!((e.create, e.modified, e.version, &*e.value), (5, 9, 2, &b"2"[..]));
        kv.delete(b"a");
        kv.put(b"a", b"3", 12);
        let e = kv.get(b"a").unwrap();
        assert_eq!((e.create, e.modified, e.version), (12, 12, 1));
    }

    #[test]
    fn ranges_cross_shards_in_key_order() {
        let mut kv = Kv::default();
        for k in ["b", "a/2", "a/1", "a/3", "c", "a\0"] {
            kv.put(k.as_bytes(), b"v", 1);
        }
        let keys = |key: &[u8], end: &[u8]| {
            kv.range(key, end).into_iter().map(|(k, _)| k.to_vec()).collect::<Vec<_>>()
        };
        assert_eq!(keys(b"a/", b"a0"), [b"a/1".to_vec(), b"a/2".to_vec(), b"a/3".to_vec()]);
        assert_eq!(keys(b"b", b"\0"), [b"b".to_vec(), b"c".to_vec()]);
        assert_eq!(keys(b"\0", b"\0").len(), 6);
        assert_eq!(keys(b"a\0", b""), [b"a\0".to_vec()]);
        assert!(keys(b"zz", b"").is_empty());
    }

    /// Random puts, deletes and snapshots taken and dropped, against a plain map: every
    /// read sees the same entries whether a write went to a shard or to its overlay.
    #[test]
    fn overlays_read_like_a_plain_map() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let contents = |kv: &Kv| kv.iter().map(|(k, e)| (k.to_vec(), e.clone())).collect::<BTreeMap<_, _>>();
        let (mut kv, mut model) = (Kv::default(), BTreeMap::<Vec<u8>, Entry>::new());
        let mut held: Vec<(Kv, BTreeMap<Vec<u8>, Entry>)> = Vec::new();
        for step in 0..20_000 {
            let key = format!("k{}", next(600)).into_bytes();
            match next(100) {
                0..=54 => {
                    let rev = kv.next_revision();
                    let value = format!("v{step}");
                    let prev = model.get(&key).cloned();
                    model.insert(key.clone(), written(prev.as_ref(), value.as_bytes().into(), rev));
                    assert_eq!(kv.put(&key, value.as_bytes(), rev), prev);
                }
                55..=84 => assert_eq!(kv.delete(&key), model.remove(&key)),
                85..=89 => assert_eq!(kv.get(&key), model.get(&key)),
                90..=93 => {
                    let end = format!("k{}", next(600)).into_bytes();
                    let want: Vec<_> = model
                        .iter()
                        .filter(|(k, _)| in_range(&key, &end, k))
                        .map(|(k, e)| (k.clone(), e.clone()))
                        .collect();
                    let got: Vec<_> =
                        kv.range(&key, &end).into_iter().map(|(k, e)| (k.to_vec(), e)).collect();
                    assert_eq!(got, want);
                }
                94..=96 if held.len() < 3 => held.push((kv.snapshot(), model.clone())),
                _ => {
                    if !held.is_empty() {
                        let (snap, then) = held.remove(next(held.len() as u64) as usize);
                        assert_eq!(contents(&snap), then);
                    }
                }
            }
            assert_eq!(kv.len(), model.len());
        }
        assert_eq!(contents(&kv), model);
        let restored = Kv::restore(kv.revision(), kv.iter().map(|(k, e)| (k.to_vec(), e.clone())));
        assert_eq!(restored, kv);
    }
}

//! The key-value state, split into shards a snapshot shares instead of copying.
//!
//! Cloning a `Kv` copies only the shard pointers, so a snapshot can be taken on the event
//! loop and encoded elsewhere; the first write to a shard after that copies just that
//! shard, and keys and values are shared rather than copied, so that copy only allocates
//! map nodes.
//!
//! Keys and values are bytes. Like etcd's store without history, the state carries a store
//! revision, raised once per write request, and each key its create revision, mod revision
//! and version.

use std::collections::BTreeMap;
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kv {
    shards: Vec<Arc<BTreeMap<Arc<[u8]>, Entry>>>,
    revision: u64,
}

impl Default for Kv {
    fn default() -> Self {
        Self { shards: vec![Arc::default(); SHARDS], revision: 0 }
    }
}

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

    pub fn get(&self, key: &[u8]) -> Option<&Entry> {
        self.shards[shard(key)].get(key)
    }

    /// Writes `value` at `revision`; returns the previous entry.
    pub fn put(&mut self, key: &[u8], value: &[u8], revision: u64) -> Option<Entry> {
        let map = Arc::make_mut(&mut self.shards[shard(key)]);
        let prev = map.get(key).cloned();
        let (create, version) = prev.as_ref().map_or((revision, 1), |p| (p.create, p.version + 1));
        map.insert(key.into(), Entry { value: value.into(), create, modified: revision, version });
        prev
    }

    pub fn delete(&mut self, key: &[u8]) -> Option<Entry> {
        let i = shard(key);
        if !self.shards[i].contains_key(key) {
            return None;
        }
        Arc::make_mut(&mut self.shards[i]).remove(key)
    }

    /// The entries in `[key, end)` (see [`in_range`]) in key order. A single key is a
    /// lookup; a span merges the shards.
    pub fn range(&self, key: &[u8], end: &[u8]) -> Vec<(Arc<[u8]>, Entry)> {
        if end.is_empty() {
            let i = shard(key);
            return self.shards[i]
                .get_key_value(key)
                .map(|(k, e)| (k.clone(), e.clone()))
                .into_iter()
                .collect();
        }
        let mut out: Vec<(Arc<[u8]>, Entry)> = self
            .shards
            .iter()
            .flat_map(|s| {
                s.range::<[u8], _>((Bound::Included(key), Bound::Unbounded))
                    .take_while(|(k, _)| in_range(key, end, k))
            })
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect();
        out.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.len()).sum()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &Entry)> {
        self.shards.iter().flat_map(|s| s.iter()).map(|(k, e)| (&**k, e))
    }

    /// A state restored from a snapshot: its revision and entries.
    pub fn restore(revision: u64, entries: impl IntoIterator<Item = (Vec<u8>, Entry)>) -> Self {
        let mut kv = Self { revision, ..Self::default() };
        for (k, e) in entries {
            Arc::make_mut(&mut kv.shards[shard(&k)]).insert(k.into(), e);
        }
        kv
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_is_a_stable_snapshot_and_writes_copy_one_shard() {
        let mut kv = Kv::default();
        for i in 0..1000 {
            let rev = kv.next_revision();
            kv.put(format!("k{i}").as_bytes(), b"a", rev);
        }
        let snapshot = kv.clone();
        let rev = kv.next_revision();
        kv.put(b"k7", b"b", rev);
        assert_eq!(
            (&*snapshot.get(b"k7").unwrap().value, &*kv.get(b"k7").unwrap().value),
            (&b"a"[..], &b"b"[..])
        );
        let shared = kv.shards.iter().zip(&snapshot.shards).filter(|(a, b)| Arc::ptr_eq(a, b)).count();
        assert_eq!((shared, kv.len(), snapshot.len()), (SHARDS - 1, 1000, 1000));
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
}

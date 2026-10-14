//! The key-value state, split into shards a snapshot shares instead of copying.
//!
//! Cloning a `Kv` copies only the shard pointers, so a snapshot can be taken on the event
//! loop and encoded elsewhere; the first write to a shard after that copies just that
//! shard, and keys and values are shared rather than copied, so that copy only allocates
//! map nodes.

use std::collections::BTreeMap;
use std::sync::Arc;

const SHARDS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kv {
    shards: Vec<Arc<BTreeMap<Arc<str>, Arc<str>>>>,
}

impl Default for Kv {
    fn default() -> Self {
        Self { shards: vec![Arc::default(); SHARDS] }
    }
}

/// FNV-1a: stable across runs and platforms, so shard placement never depends on a seed.
fn shard(key: &str) -> usize {
    let h =
        key.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    (h % SHARDS as u64) as usize
}

impl Kv {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.shards[shard(key)].get(key).map(|v| &**v)
    }

    pub fn insert(&mut self, key: String, value: String) {
        Arc::make_mut(&mut self.shards[shard(&key)]).insert(key.into(), value.into());
    }

    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.len()).sum()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.shards.iter().flat_map(|s| s.iter()).map(|(k, v)| (&**k, &**v))
    }
}

impl FromIterator<(String, String)> for Kv {
    fn from_iter<I: IntoIterator<Item = (String, String)>>(pairs: I) -> Self {
        let mut kv = Self::default();
        for (k, v) in pairs {
            kv.insert(k, v);
        }
        kv
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_is_a_stable_snapshot_and_writes_copy_one_shard() {
        let mut kv: Kv = (0..1000).map(|i| (format!("k{i}"), "a".to_owned())).collect();
        let snapshot = kv.clone();
        kv.insert("k7".to_owned(), "b".to_owned());
        assert_eq!((snapshot.get("k7"), kv.get("k7")), (Some("a"), Some("b")));
        let shared = kv.shards.iter().zip(&snapshot.shards).filter(|(a, b)| Arc::ptr_eq(a, b)).count();
        assert_eq!((shared, kv.len(), snapshot.len()), (SHARDS - 1, 1000, 1000));
    }
}

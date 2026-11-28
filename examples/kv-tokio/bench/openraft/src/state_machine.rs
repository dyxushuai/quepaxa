//! In-memory KV state machine with atomic durable snapshots (`snapshot.bin`), implementing
//! Openraft's [`RaftStateMachine`] and [`RaftSnapshotBuilder`].

use std::collections::BTreeMap;
use std::collections::btree_map::Entry as MapEntry;
use std::fs::{self, File};
use std::io::{self, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use futures_util::{FutureExt, Stream, StreamExt};
use openraft::EntryPayload;
use openraft::entry::RaftEntry;
use openraft::storage::{EntryResponder, RaftSnapshotBuilder, RaftStateMachine};

use crate::codec::{
    SnapshotPayload, decode_disk_snapshot, decode_snapshot_payload, encode_disk_snapshot,
    encode_snapshot_payload,
};
use crate::store::sync_dir;
pub use crate::types::KvEntry;
use crate::types::{
    CmpOp, CmpTarget, KvOpResult, KvRequest, KvResponse, KvResponseKind, LogId, RangeQuery, Snapshot,
    SnapshotMeta, StoredMembership, TxnCompare, TxnOp, TypeConfig,
};

#[derive(Default)]
struct SmInner {
    last_applied: Option<LogId>,
    last_membership: StoredMembership,
    revision: u64,
    kvs: BTreeMap<Vec<u8>, KvEntry>,
    current_snapshot: Option<StoredSnapshot>,
}

#[derive(Clone)]
struct StoredSnapshot {
    meta: SnapshotMeta,
    data: Vec<u8>,
}

/// Shared KV state machine handle for both Openraft and the client gRPC front end.
#[derive(Clone)]
pub struct StateMachine {
    dir: PathBuf,
    inner: Arc<RwLock<SmInner>>,
}

impl StateMachine {
    /// Loads any existing durable snapshot from `<dir>/snapshot.bin` or starts empty.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let mut inner = SmInner::default();
        let snap_path = dir.join("snapshot.bin");
        match fs::read(&snap_path) {
            Ok(bytes) => {
                let (meta, data) = decode_disk_snapshot(&bytes)?;
                let payload = decode_snapshot_payload(&data)?;
                inner.last_applied = payload.last_applied;
                inner.last_membership = payload.last_membership;
                inner.revision = payload.revision;
                inner.kvs = payload.kvs.into_iter().collect();
                inner.current_snapshot = Some(StoredSnapshot { meta, data });
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(Self { dir: dir.to_path_buf(), inner: Arc::new(RwLock::new(inner)) })
    }

    /// Reads one key (with per-key revision metadata) and the current store revision.
    pub fn get(&self, key: &[u8]) -> (u64, Option<KvEntry>) {
        let inner = self.inner.read().expect("state machine lock poisoned");
        (inner.revision, inner.kvs.get(key).cloned())
    }

    /// Executes an etcd `Range` query against the current in-memory state.
    pub fn range(&self, q: &RangeQuery) -> (u64, Vec<(Vec<u8>, KvEntry)>, bool, u64) {
        let inner = self.inner.read().expect("state machine lock poisoned");
        let (kvs, more, count) = eval_range(&inner.kvs, q);
        (inner.revision, kvs, more, count)
    }
}

fn eval_range(kvs: &BTreeMap<Vec<u8>, KvEntry>, q: &RangeQuery) -> (Vec<(Vec<u8>, KvEntry)>, bool, u64) {
    if q.range_end.is_empty() {
        return match kvs.get(&q.key) {
            Some(entry) => {
                let list = if q.count_only {
                    Vec::new()
                } else {
                    let mut e = entry.clone();
                    if q.keys_only {
                        e.value.clear();
                    }
                    vec![(q.key.clone(), e)]
                };
                (list, false, 1)
            }
            None => (Vec::new(), false, 0),
        };
    }
    let bounds = if q.range_end == [0] {
        (Bound::Included(q.key.as_slice()), Bound::Unbounded)
    } else if q.range_end <= q.key {
        return (Vec::new(), false, 0);
    } else {
        (Bound::Included(q.key.as_slice()), Bound::Excluded(q.range_end.as_slice()))
    };
    let mut total = 0u64;
    let mut out = Vec::new();
    for (k, entry) in kvs.range::<[u8], _>(bounds) {
        total += 1;
        if !q.count_only && (q.limit == 0 || (out.len() as u64) < q.limit) {
            let mut e = entry.clone();
            if q.keys_only {
                e.value.clear();
            }
            out.push((k.clone(), e));
        }
    }
    let more = !q.count_only && q.limit > 0 && total > q.limit;
    (out, more, total)
}

fn eval_order<T: Ord>(a: T, b: T, op: CmpOp) -> bool {
    match op {
        CmpOp::Equal => a == b,
        CmpOp::NotEqual => a != b,
        CmpOp::Less => a < b,
        CmpOp::Greater => a > b,
    }
}

fn compare_holds(kvs: &BTreeMap<Vec<u8>, KvEntry>, c: &TxnCompare) -> bool {
    let e = kvs.get(&c.key);
    match &c.target {
        CmpTarget::Value(v) => e.is_some_and(|e| eval_order(e.value.as_slice(), v.as_slice(), c.op)),
        CmpTarget::Version(n) => eval_order(e.map_or(0, |e| e.version), *n, c.op),
        CmpTarget::Create(n) => eval_order(e.map_or(0, |e| e.create_revision), *n, c.op),
        CmpTarget::Mod(n) => eval_order(e.map_or(0, |e| e.mod_revision), *n, c.op),
    }
}

fn apply_put(
    kvs: &mut BTreeMap<Vec<u8>, KvEntry>,
    rev: u64,
    key: Vec<u8>,
    value: Vec<u8>,
    prev_kv: bool,
) -> Option<(Vec<u8>, KvEntry)> {
    let prev_key = prev_kv.then(|| key.clone());
    match kvs.entry(key) {
        MapEntry::Occupied(mut o) => {
            let old = prev_kv.then(|| o.get().clone());
            let e = o.get_mut();
            e.value = value;
            e.mod_revision = rev;
            e.version += 1;
            prev_key.zip(old)
        }
        MapEntry::Vacant(v) => {
            v.insert(KvEntry { value, create_revision: rev, mod_revision: rev, version: 1 });
            None
        }
    }
}

fn delete_keys(kvs: &BTreeMap<Vec<u8>, KvEntry>, key: &[u8], range_end: &[u8]) -> Vec<Vec<u8>> {
    if range_end.is_empty() {
        return if kvs.contains_key(key) { vec![key.to_vec()] } else { Vec::new() };
    }
    if range_end == [0] {
        return kvs
            .range::<[u8], _>((Bound::Included(key), Bound::Unbounded))
            .map(|(k, _)| k.clone())
            .collect();
    }
    if range_end <= key {
        return Vec::new();
    }
    kvs.range::<[u8], _>((Bound::Included(key), Bound::Excluded(range_end))).map(|(k, _)| k.clone()).collect()
}

fn apply_delete(
    kvs: &mut BTreeMap<Vec<u8>, KvEntry>,
    key: &[u8],
    range_end: &[u8],
    prev_kv: bool,
) -> (u64, Vec<(Vec<u8>, KvEntry)>) {
    let targets = delete_keys(kvs, key, range_end);
    let mut prev = if prev_kv { Vec::with_capacity(targets.len()) } else { Vec::new() };
    let mut deleted = 0u64;
    for k in targets {
        if let Some(old) = kvs.remove(&k) {
            deleted += 1;
            if prev_kv {
                prev.push((k, old));
            }
        }
    }
    (deleted, prev)
}

fn branch_mutates(kvs: &BTreeMap<Vec<u8>, KvEntry>, ops: &[TxnOp]) -> bool {
    ops.iter().any(|op| match op {
        TxnOp::Put { .. } => true,
        TxnOp::Delete { key, range_end, .. } => !delete_keys(kvs, key, range_end).is_empty(),
        TxnOp::Range(_) => false,
    })
}

fn apply_kv_request(inner: &mut SmInner, req: KvRequest) -> KvResponse {
    match req {
        KvRequest::Put { key, value, prev_kv } => {
            inner.revision += 1;
            let rev = inner.revision;
            let prev = apply_put(&mut inner.kvs, rev, key, value, prev_kv);
            KvResponse { revision: rev, kind: KvResponseKind::Put { prev } }
        }
        KvRequest::DeleteRange { key, range_end, prev_kv } => {
            let (deleted, prev) = apply_delete(&mut inner.kvs, &key, &range_end, prev_kv);
            if deleted > 0 {
                inner.revision += 1;
            }
            KvResponse { revision: inner.revision, kind: KvResponseKind::Delete { deleted, prev } }
        }
        KvRequest::Txn { compares, success, failure } => {
            let succeeded = compares.iter().all(|c| compare_holds(&inner.kvs, c));
            let branch = if succeeded { success } else { failure };
            if branch_mutates(&inner.kvs, &branch) {
                inner.revision += 1;
            }
            let rev = inner.revision;
            let mut responses = Vec::with_capacity(branch.len());
            for op in branch {
                match op {
                    TxnOp::Range(q) => {
                        let (kvs, more, count) = eval_range(&inner.kvs, &q);
                        responses.push(KvOpResult::Range { kvs, more, count });
                    }
                    TxnOp::Put { key, value, prev_kv } => {
                        let prev = apply_put(&mut inner.kvs, rev, key, value, prev_kv);
                        responses.push(KvOpResult::Put { prev });
                    }
                    TxnOp::Delete { key, range_end, prev_kv } => {
                        let (deleted, prev) = apply_delete(&mut inner.kvs, &key, &range_end, prev_kv);
                        responses.push(KvOpResult::Delete { deleted, prev });
                    }
                }
            }
            KvResponse { revision: rev, kind: KvResponseKind::Txn { succeeded, responses } }
        }
    }
}

fn persist_snapshot(dir: &Path, meta: &SnapshotMeta, data: &[u8]) -> io::Result<()> {
    let encoded = encode_disk_snapshot(meta, data)?;
    let tmp = dir.join("snapshot.tmp");
    let mut f = File::create(&tmp)?;
    f.write_all(&encoded)?;
    f.sync_all()?;
    fs::rename(&tmp, dir.join("snapshot.bin"))?;
    sync_dir(dir)?;
    Ok(())
}

impl RaftSnapshotBuilder<TypeConfig> for StateMachine {
    type SnapshotData = Vec<u8>;

    async fn build_snapshot(&mut self) -> Result<Snapshot, io::Error> {
        let (meta, payload) = {
            let inner = self.inner.read().expect("state machine lock poisoned");
            let meta = SnapshotMeta {
                last_log_id: inner.last_applied,
                last_membership: inner.last_membership.clone(),
            };
            let payload = SnapshotPayload {
                last_applied: inner.last_applied,
                last_membership: inner.last_membership.clone(),
                revision: inner.revision,
                kvs: inner.kvs.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            };
            (meta, payload)
        };

        let dir = self.dir.clone();
        let meta_for_disk = meta.clone();
        let data = tokio::task::spawn_blocking(move || -> io::Result<Vec<u8>> {
            let data = encode_snapshot_payload(&payload)?;
            persist_snapshot(&dir, &meta_for_disk, &data)?;
            Ok(data)
        })
        .await
        .map_err(|e| io::Error::other(format!("snapshot task failed: {e}")))??;

        self.inner.write().expect("state machine lock poisoned").current_snapshot =
            Some(StoredSnapshot { meta: meta.clone(), data: data.clone() });

        Ok(Snapshot { meta, snapshot: data })
    }
}

impl RaftStateMachine<TypeConfig> for StateMachine {
    type SnapshotData = Vec<u8>;
    type SnapshotBuilder = Self;

    async fn applied_state(&mut self) -> Result<(Option<LogId>, StoredMembership), io::Error> {
        let inner = self.inner.read().expect("state machine lock poisoned");
        Ok((inner.last_applied, inner.last_membership.clone()))
    }

    async fn apply<Strm>(&mut self, mut entries: Strm) -> Result<(), io::Error>
    where
        Strm: Stream<Item = Result<EntryResponder<TypeConfig>, io::Error>> + Unpin + Send,
    {
        let mut batch = Vec::with_capacity(64);
        let mut replies = Vec::with_capacity(64);
        while let Some(first) = entries.next().await {
            batch.push(first?);
            while let Some(Some(more)) = entries.next().now_or_never() {
                batch.push(more?);
            }
            {
                let mut inner = self.inner.write().expect("state machine lock poisoned");
                for (entry, responder) in batch.drain(..) {
                    let log_id = entry.log_id();
                    inner.last_applied = Some(log_id);
                    let resp = match entry.payload {
                        EntryPayload::Blank => {
                            KvResponse { revision: inner.revision, kind: KvResponseKind::default() }
                        }
                        EntryPayload::Normal(req) => apply_kv_request(&mut inner, req),
                        EntryPayload::Membership(mem) => {
                            inner.last_membership = StoredMembership::new(Some(log_id), mem);
                            KvResponse { revision: inner.revision, kind: KvResponseKind::default() }
                        }
                    };
                    if let Some(tx) = responder {
                        replies.push((tx, resp));
                    }
                }
            }
            for (tx, resp) in replies.drain(..) {
                tx.send(resp);
            }
        }
        Ok(())
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn install_snapshot(&mut self, meta: &SnapshotMeta, snapshot: Vec<u8>) -> Result<(), io::Error> {
        let payload = decode_snapshot_payload(&snapshot)?;
        let dir = self.dir.clone();
        let meta_clone = meta.clone();
        let snap_clone = snapshot.clone();
        tokio::task::spawn_blocking(move || persist_snapshot(&dir, &meta_clone, &snap_clone))
            .await
            .map_err(|e| io::Error::other(format!("install_snapshot task failed: {e}")))??;

        let mut inner = self.inner.write().expect("state machine lock poisoned");
        inner.last_applied = meta.last_log_id;
        inner.last_membership = meta.last_membership.clone();
        inner.revision = payload.revision;
        inner.kvs = payload.kvs.into_iter().collect();
        inner.current_snapshot = Some(StoredSnapshot { meta: meta.clone(), data: snapshot });
        Ok(())
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<Snapshot>, io::Error> {
        let inner = self.inner.read().expect("state machine lock poisoned");
        Ok(inner
            .current_snapshot
            .as_ref()
            .map(|s| Snapshot { meta: s.meta.clone(), snapshot: s.data.clone() }))
    }
}

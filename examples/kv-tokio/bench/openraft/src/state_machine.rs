//! In-memory KV state machine with atomic durable snapshots (`snapshot.bin`), implementing
//! Openraft's [`RaftStateMachine`] and [`RaftSnapshotBuilder`].

use std::collections::BTreeMap;
use std::collections::btree_map::Entry as MapEntry;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use futures_util::{Stream, StreamExt};
use openraft::EntryPayload;
use openraft::entry::RaftEntry;
use openraft::storage::{EntryResponder, RaftSnapshotBuilder, RaftStateMachine};
use serde::{Deserialize, Serialize};

use crate::store::sync_dir;
use crate::types::{KvRequest, KvResponse, LogId, Snapshot, SnapshotMeta, StoredMembership, TypeConfig};

/// Stored value plus etcd-compatible per-key revision metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KvEntry {
    pub value: Vec<u8>,
    pub create_revision: u64,
    pub mod_revision: u64,
    pub version: u64,
}

#[derive(Serialize, Deserialize)]
struct SnapshotPayload {
    last_applied: Option<LogId>,
    last_membership: StoredMembership,
    revision: u64,
    kvs: Vec<(Vec<u8>, KvEntry)>,
}

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
                let payload: SnapshotPayload = serde_json::from_slice(&data)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
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
}

#[derive(Serialize, Deserialize)]
struct DiskSnapshot {
    meta: SnapshotMeta,
    data: Vec<u8>,
}

fn encode_disk_snapshot(meta: &SnapshotMeta, data: &[u8]) -> io::Result<Vec<u8>> {
    let body = serde_json::to_vec(&DiskSnapshot { meta: meta.clone(), data: data.to_vec() })
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32fast::hash(&body).to_le_bytes());
    Ok(out)
}

fn decode_disk_snapshot(bytes: &[u8]) -> io::Result<(SnapshotMeta, Vec<u8>)> {
    let body_len = bytes
        .len()
        .checked_sub(4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "snapshot too short"))?;
    let (body, crc_bytes) = bytes.split_at(body_len);
    let want = u32::from_le_bytes(crc_bytes.try_into().expect("4 bytes"));
    if crc32fast::hash(body) != want {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "snapshot checksum mismatch"));
    }
    let disk: DiskSnapshot =
        serde_json::from_slice(body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok((disk.meta, disk.data))
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
            let data =
                serde_json::to_vec(&payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
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
        while let Some(item) = entries.next().await {
            let (entry, responder) = item?;
            let log_id = entry.log_id();
            let revision = {
                let mut inner = self.inner.write().expect("state machine lock poisoned");
                inner.last_applied = Some(log_id);
                match entry.payload {
                    EntryPayload::Blank => inner.revision,
                    EntryPayload::Normal(KvRequest::Put { key, value }) => {
                        inner.revision += 1;
                        let rev = inner.revision;
                        match inner.kvs.entry(key) {
                            MapEntry::Occupied(mut o) => {
                                let e = o.get_mut();
                                e.value = value;
                                e.mod_revision = rev;
                                e.version += 1;
                            }
                            MapEntry::Vacant(v) => {
                                v.insert(KvEntry {
                                    value,
                                    create_revision: rev,
                                    mod_revision: rev,
                                    version: 1,
                                });
                            }
                        }
                        rev
                    }
                    EntryPayload::Membership(mem) => {
                        inner.last_membership = StoredMembership::new(Some(log_id), mem);
                        inner.revision
                    }
                }
            };
            if let Some(tx) = responder {
                tx.send(KvResponse { revision });
            }
        }
        Ok(())
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn install_snapshot(&mut self, meta: &SnapshotMeta, snapshot: Vec<u8>) -> Result<(), io::Error> {
        let payload: SnapshotPayload =
            serde_json::from_slice(&snapshot).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
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

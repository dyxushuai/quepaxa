//! Durable Raft log storage backed by an append-only CRC32-framed WAL (`raft.log`) flushed
//! with `sync_data()` on a dedicated group-commit writer thread, matching `kv-tokio`'s disk
//! durability model.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::ops::RangeBounds;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, mpsc};
use std::thread;

use openraft::entry::RaftEntry;
use openraft::storage::{IOFlushed, LogState, RaftLogReader, RaftLogStorage};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::types::{Entry, LogId, TypeConfig, Vote};

const HEADER: usize = 12;
const SECTOR: usize = 512;

#[derive(Serialize, Deserialize)]
enum WalRecord {
    Vote(Vote),
    Append(Vec<Entry>),
    Truncate(Option<LogId>),
    Purge(LogId),
}

enum WriteOp {
    Vote { bytes: Vec<u8>, done: oneshot::Sender<io::Result<()>> },
    Append { bytes: Vec<u8>, callback: IOFlushed<TypeConfig> },
    Sync { bytes: Vec<u8>, done: oneshot::Sender<io::Result<()>> },
}

#[derive(Default)]
struct LogInner {
    vote: Option<Vote>,
    last_purged: Option<LogId>,
    entries: BTreeMap<u64, Entry>,
}

/// Durable log storage implementing [`RaftLogStorage`] and [`RaftLogReader`].
#[derive(Clone)]
pub struct LogStore {
    inner: Arc<RwLock<LogInner>>,
    writer: mpsc::Sender<WriteOp>,
}

impl LogStore {
    /// Opens (creating if needed) `<dir>/raft.log`, replays valid frames, truncates any torn
    /// tail, and starts the dedicated group-commit fsync writer thread.
    pub fn open(dir: &Path) -> io::Result<Self> {
        create_dir_durably(dir)?;
        let path = dir.join("raft.log");
        let bytes = match fs::read(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            r => r?,
        };
        let (frames, valid) = read_frames(&bytes)?;
        if valid < bytes.len() {
            let f = OpenOptions::new().write(true).open(&path)?;
            f.set_len(valid as u64)?;
            f.sync_all()?;
        }

        let mut inner = LogInner::default();
        for raw in frames {
            let rec: WalRecord =
                serde_json::from_slice(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            apply_record(&mut inner, rec);
        }

        let log = OpenOptions::new().create(true).append(true).open(&path)?;
        log.sync_all()?;
        sync_dir(dir)?;

        let (tx, rx) = mpsc::channel();
        thread::Builder::new().name("openraft-wal".to_owned()).spawn(move || writer_loop(log, rx))?;

        Ok(Self { inner: Arc::new(RwLock::new(inner)), writer: tx })
    }
}

fn apply_record(inner: &mut LogInner, rec: WalRecord) {
    match rec {
        WalRecord::Vote(v) => inner.vote = Some(v),
        WalRecord::Append(entries) => {
            for e in entries {
                inner.entries.insert(e.log_id().index, e);
            }
        }
        WalRecord::Truncate(last) => match last {
            None => inner.entries.clear(),
            Some(id) => {
                inner.entries.split_off(&(id.index + 1));
            }
        },
        WalRecord::Purge(id) => {
            inner.last_purged = Some(id);
            inner.entries = inner.entries.split_off(&(id.index + 1));
        }
    }
}

fn encode_frame(rec: &WalRecord) -> io::Result<Vec<u8>> {
    let payload = serde_json::to_vec(rec).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = (payload.len() as u32).to_le_bytes();
    let mut buf = Vec::with_capacity(HEADER + payload.len());
    buf.extend_from_slice(&len);
    buf.extend_from_slice(&crc32fast::hash(&len).to_le_bytes());
    buf.extend_from_slice(&crc32fast::hash(&payload).to_le_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

/// Parses CRC32-framed records, truncating a torn tail at EOF or zero-filled sectors while
/// rejecting mid-log corruption.
fn read_frames(bytes: &[u8]) -> io::Result<(Vec<Vec<u8>>, usize)> {
    let zero_run = bytes.iter().rposition(|&b| b != 0).map_or(0, |last| last + 1);
    let zeros = zero_run.next_multiple_of(SECTOR);
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        let rest = &bytes[pos..];
        if rest.len() < HEADER {
            break;
        }
        let corrupt =
            || io::Error::new(io::ErrorKind::InvalidData, format!("raft.log is corrupt at byte {pos}"));
        if crc32fast::hash(&rest[..4]) != u32_at(rest, 4) {
            if zero_run <= pos || zeros < pos + HEADER {
                break;
            }
            return Err(corrupt());
        }
        let end = HEADER + u32_at(rest, 0) as usize;
        let Some(payload) = rest.get(HEADER..end) else { break };
        if crc32fast::hash(payload) != u32_at(rest, 8) {
            if zeros < pos + end {
                break;
            }
            return Err(corrupt());
        }
        out.push(payload.to_vec());
        pos += end;
    }
    Ok((out, pos))
}

/// Group-commit writer loop: drains all queued WAL records, writes them in one syscall,
/// calls `sync_data()` once, and notifies all waiters in submission order.
fn writer_loop(mut log: File, rx: mpsc::Receiver<WriteOp>) {
    let mut batch = Vec::with_capacity(64);
    let mut buf = Vec::with_capacity(64 * 1024);
    while let Ok(first) = rx.recv() {
        batch.push(first);
        while let Ok(more) = rx.try_recv() {
            batch.push(more);
        }
        buf.clear();
        for op in &batch {
            let bytes = match op {
                WriteOp::Vote { bytes, .. } | WriteOp::Append { bytes, .. } | WriteOp::Sync { bytes, .. } => {
                    bytes
                }
            };
            buf.extend_from_slice(bytes);
        }
        let res = log.write_all(&buf).and_then(|()| log.sync_data());
        for op in batch.drain(..) {
            let item_res = match &res {
                Ok(()) => Ok(()),
                Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
            };
            match op {
                WriteOp::Vote { done, .. } | WriteOp::Sync { done, .. } => {
                    let _ = done.send(item_res);
                }
                WriteOp::Append { callback, .. } => {
                    callback.io_completed(item_res);
                }
            }
        }
    }
}

pub(crate) fn create_dir_durably(dir: &Path) -> io::Result<()> {
    let dir = std::path::absolute(dir)?;
    let created: Vec<PathBuf> = dir.ancestors().take_while(|d| !d.exists()).map(Path::to_path_buf).collect();
    fs::create_dir_all(&dir)?;
    for parent in dir.ancestors().skip(1) {
        match sync_dir(parent) {
            Err(e)
                if e.kind() == io::ErrorKind::PermissionDenied
                    && !created.iter().any(|c| c.parent() == Some(parent)) => {}
            r => r?,
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
pub(crate) fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

impl RaftLogReader<TypeConfig> for LogStore {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry>, io::Error> {
        let inner = self.inner.read().expect("log lock poisoned");
        Ok(inner.entries.range(range).map(|(_, e)| e.clone()).collect())
    }

    async fn read_vote(&mut self) -> Result<Option<Vote>, io::Error> {
        Ok(self.inner.read().expect("log lock poisoned").vote)
    }
}

impl RaftLogStorage<TypeConfig> for LogStore {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, io::Error> {
        let inner = self.inner.read().expect("log lock poisoned");
        let last_log_id = inner.entries.iter().next_back().map(|(_, e)| e.log_id()).or(inner.last_purged);
        Ok(LogState { last_purged_log_id: inner.last_purged, last_log_id })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote) -> Result<(), io::Error> {
        self.inner.write().expect("log lock poisoned").vote = Some(*vote);
        let bytes = encode_frame(&WalRecord::Vote(*vote))?;
        let (tx, rx) = oneshot::channel();
        self.writer
            .send(WriteOp::Vote { bytes, done: tx })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "wal writer stopped"))?;
        rx.await.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "wal writer dropped reply"))?
    }

    async fn append<I>(&mut self, entries: I, callback: IOFlushed<TypeConfig>) -> Result<(), io::Error>
    where
        I: IntoIterator<Item = Entry> + Send,
        I::IntoIter: Send,
    {
        let batch: Vec<Entry> = entries.into_iter().collect();
        let bytes = encode_frame(&WalRecord::Append(batch.clone()))?;
        {
            let mut inner = self.inner.write().expect("log lock poisoned");
            for e in batch {
                inner.entries.insert(e.log_id().index, e);
            }
        }
        self.writer
            .send(WriteOp::Append { bytes, callback })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "wal writer stopped"))?;
        Ok(())
    }

    async fn truncate_after(&mut self, last_log_id: Option<LogId>) -> Result<(), io::Error> {
        {
            let mut inner = self.inner.write().expect("log lock poisoned");
            match last_log_id {
                None => inner.entries.clear(),
                Some(id) => {
                    inner.entries.split_off(&(id.index + 1));
                }
            }
        }
        let bytes = encode_frame(&WalRecord::Truncate(last_log_id))?;
        let (tx, rx) = oneshot::channel();
        self.writer
            .send(WriteOp::Sync { bytes, done: tx })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "wal writer stopped"))?;
        rx.await.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "wal writer dropped reply"))?
    }

    async fn purge(&mut self, log_id: LogId) -> Result<(), io::Error> {
        {
            let mut inner = self.inner.write().expect("log lock poisoned");
            inner.last_purged = Some(log_id);
            inner.entries = inner.entries.split_off(&(log_id.index + 1));
        }
        let bytes = encode_frame(&WalRecord::Purge(log_id))?;
        let (tx, rx) = oneshot::channel();
        self.writer
            .send(WriteOp::Sync { bytes, done: tx })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "wal writer stopped"))?;
        rx.await.map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "wal writer dropped reply"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::EntryPayload;
    use openraft::type_config::TypeConfigExt;
    use openraft::vote::RaftLeaderId;

    use crate::types::KvRequest;

    fn temp_dir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("openraft-kv-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    #[tokio::test]
    async fn wal_persists_vote_append_truncate_and_recovers_torn_tail() {
        let dir = temp_dir("wal");
        let mut store = LogStore::open(&dir).expect("open fresh wal");
        let vote = Vote::new(2, 1);
        store.save_vote(&vote).await.expect("save vote");

        let e1 = Entry {
            log_id: LogId::new(crate::types::CommittedLeaderId::new(2, 1), 1),
            payload: EntryPayload::Normal(KvRequest::Put { key: b"k1".to_vec(), value: b"v1".to_vec() }),
        };
        let e2 = Entry {
            log_id: LogId::new(crate::types::CommittedLeaderId::new(2, 1), 2),
            payload: EntryPayload::Normal(KvRequest::Put { key: b"k2".to_vec(), value: b"v2".to_vec() }),
        };
        let (tx, rx) = TypeConfig::oneshot();
        store.append([e1.clone(), e2], IOFlushed::signal(tx)).await.expect("append");
        rx.await.expect("flush signal").expect("fsync ok");
        store.truncate_after(Some(e1.log_id())).await.expect("truncate");
        drop(store);

        // Append a torn tail to simulate crash mid-write.
        let mut f = OpenOptions::new().append(true).open(dir.join("raft.log")).expect("open");
        f.write_all(&[0xde, 0xad, 0xbe, 0xef, 0x00]).expect("write torn tail");
        drop(f);

        let mut reopened = LogStore::open(&dir).expect("reopen wal");
        assert_eq!(reopened.read_vote().await.expect("vote"), Some(vote));
        let entries = reopened.try_get_log_entries(1..=5).await.expect("entries");
        assert_eq!(entries, vec![e1]);
        let _ = fs::remove_dir_all(&dir);
    }
}

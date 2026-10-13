//! Durable storage for one replica: an append-only record log plus an application
//! snapshot, both on local disk with fsync.
//!
//! Log format: `[u32 little-endian length][record]` frames. A torn final frame (crash
//! mid-append) is truncated on load, as any write-ahead log does; record integrity beyond
//! framing is checked by the replica's own CRC.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use bytes::Bytes;

/// The application state persisted with a snapshot.
pub struct Snapshot {
    /// Last slot the state covers.
    pub through: u64,
    /// `Replica::core_state` taken right after applying `through`.
    pub core: Bytes,
    /// The key-value map.
    pub kv: BTreeMap<String, String>,
}

pub struct Disk {
    dir: PathBuf,
    log: File,
}

/// Makes directory entries (created or renamed files) durable.
#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Windows cannot open a directory as a file to flush it; NTFS journals the rename and
/// `MoveFileEx` semantics keep the old or the new file, so file-level syncs suffice there.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

fn read_frames(bytes: &[u8]) -> (Vec<Bytes>, usize) {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(header) = bytes.get(pos..pos + 4) {
        let len = u32::from_le_bytes(header.try_into().expect("4 bytes")) as usize;
        let Some(record) = bytes.get(pos + 4..pos + 4 + len) else { break };
        out.push(Bytes::copy_from_slice(record));
        pos += 4 + len;
    }
    (out, pos)
}

fn frame(records: &[Bytes]) -> Vec<u8> {
    let mut buf = Vec::new();
    for r in records {
        buf.extend_from_slice(&(r.len() as u32).to_le_bytes());
        buf.extend_from_slice(r);
    }
    buf
}

impl Disk {
    /// Opens (creating) the data directory; returns the durable records in write order.
    pub fn open(dir: &Path) -> io::Result<(Self, Vec<Bytes>)> {
        fs::create_dir_all(dir)?;
        let path = dir.join("records.log");
        let mut bytes = Vec::new();
        let existed = path.exists();
        if existed {
            File::open(&path)?.read_to_end(&mut bytes)?;
        }
        let (records, valid) = read_frames(&bytes);
        let log = OpenOptions::new().create(true).append(true).open(&path)?;
        if !existed {
            // The new log's directory entry must survive a crash before anything is acknowledged.
            log.sync_all()?;
            sync_dir(dir)?;
            if let Some(parent) = dir.parent() {
                sync_dir(parent)?;
            }
        }
        if valid < bytes.len() {
            log.set_len(valid as u64)?;
            log.sync_all()?;
        }
        Ok((Self { dir: dir.to_path_buf(), log }, records))
    }

    /// Appends records durably (one fsync for the whole group).
    pub fn append(&mut self, records: &[Bytes]) -> io::Result<()> {
        self.log.write_all(&frame(records))?;
        self.log.sync_data()
    }

    /// Atomically replaces the whole log (write new file, fsync, rename, fsync directory).
    pub fn replace(&mut self, records: &[Bytes]) -> io::Result<()> {
        let tmp = self.dir.join("records.tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(&frame(records))?;
        f.sync_all()?;
        fs::rename(&tmp, self.dir.join("records.log"))?;
        sync_dir(&self.dir)?;
        self.log = OpenOptions::new().append(true).open(self.dir.join("records.log"))?;
        Ok(())
    }

    pub fn save_snapshot(&self, snap: &Snapshot) -> io::Result<()> {
        let tmp = self.dir.join("snapshot.tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(&encode_snapshot(snap))?;
        f.sync_all()?;
        fs::rename(&tmp, self.dir.join("snapshot.bin"))?;
        sync_dir(&self.dir)
    }

    pub fn load_snapshot(&self) -> io::Result<Option<Bytes>> {
        match fs::read(self.dir.join("snapshot.bin")) {
            Ok(b) => Ok(Some(Bytes::from(b))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// `[through u64][core len u32][core][count u32]([klen u32][k][vlen u32][v])*`.
pub fn encode_snapshot(s: &Snapshot) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&s.through.to_le_bytes());
    b.extend_from_slice(&(s.core.len() as u32).to_le_bytes());
    b.extend_from_slice(&s.core);
    b.extend_from_slice(&(s.kv.len() as u32).to_le_bytes());
    for (k, v) in &s.kv {
        for part in [k.as_bytes(), v.as_bytes()] {
            b.extend_from_slice(&(part.len() as u32).to_le_bytes());
            b.extend_from_slice(part);
        }
    }
    b
}

pub fn decode_snapshot(b: &Bytes) -> Option<Snapshot> {
    let mut pos = 0usize;
    let mut take = |n: usize| {
        let s = b.get(pos..pos.checked_add(n)?)?;
        pos += n;
        Some(s)
    };
    let through = u64::from_le_bytes(take(8)?.try_into().ok()?);
    let core_len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
    let core = Bytes::copy_from_slice(take(core_len)?);
    let count = u32::from_le_bytes(take(4)?.try_into().ok()?);
    let mut kv = BTreeMap::new();
    for _ in 0..count {
        let mut part = || {
            let len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            String::from_utf8(take(len)?.to_vec()).ok()
        };
        let (k, v) = (part()?, part()?);
        kv.insert(k, v);
    }
    Some(Snapshot { through, core, kv })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("kv-tokio-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn records_survive_reopen_and_a_torn_tail_is_dropped() {
        let dir = temp_dir("torn");
        let (mut disk, none) = Disk::open(&dir).unwrap();
        assert!(none.is_empty());
        disk.append(&[Bytes::from_static(b"a"), Bytes::from_static(b"bc")]).unwrap();
        drop(disk);
        let mut f = OpenOptions::new().append(true).open(dir.join("records.log")).unwrap();
        f.write_all(&[9, 0, 0, 0, 1]).unwrap(); // a frame cut short by a crash
        let (_, records) = Disk::open(&dir).unwrap();
        assert_eq!(records, vec![Bytes::from_static(b"a"), Bytes::from_static(b"bc")]);
        let (_, again) = Disk::open(&dir).unwrap();
        assert_eq!(again.len(), 2, "the torn tail was truncated");
    }

    #[test]
    fn replace_swaps_the_whole_log() {
        let dir = temp_dir("replace");
        let (mut disk, _) = Disk::open(&dir).unwrap();
        disk.append(&[Bytes::from_static(b"old")]).unwrap();
        disk.replace(&[Bytes::from_static(b"new")]).unwrap();
        disk.append(&[Bytes::from_static(b"next")]).unwrap();
        let (_, records) = Disk::open(&dir).unwrap();
        assert_eq!(records, vec![Bytes::from_static(b"new"), Bytes::from_static(b"next")]);
    }

    #[test]
    fn snapshot_round_trips() {
        let kv = BTreeMap::from([("k".to_owned(), "v".to_owned())]);
        let s = Snapshot { through: 7, core: Bytes::from_static(b"core"), kv };
        let d = decode_snapshot(&Bytes::from(encode_snapshot(&s))).unwrap();
        assert_eq!((d.through, d.core, d.kv), (s.through, s.core, s.kv));
    }
}

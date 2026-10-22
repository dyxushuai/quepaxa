//! Durable storage for one replica: an append-only record log plus an application
//! snapshot, both on local disk with fsync.
//!
//! Log format: `[len u32][crc32 of len u32][crc32 of record u32][record]` frames, little
//! endian. On load, a torn tail (a crash mid-append: a frame cut short, or a write that
//! never reached disk and reads back as zeros from where the append or a sector began) is
//! truncated, as any write-ahead log does.
//! Anything else that fails a checksum is corruption, and the log is left alone: truncating
//! there would silently drop records that were acknowledged.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;

use crate::kv::Kv;

/// The application state persisted with a snapshot.
pub struct Snapshot {
    /// Last slot the state covers.
    pub through: u64,
    /// `Replica::core_state` taken right after applying `through`.
    pub core: Bytes,
    /// The key-value map.
    pub kv: Kv,
}

pub struct Disk {
    dir: PathBuf,
    log: File,
    snapshots: SnapshotWriter,
}

/// Writes application snapshots, from any thread, never replacing a newer one with an
/// older one: the log below a saved snapshot may already be gone.
#[derive(Clone)]
pub struct SnapshotWriter {
    dir: PathBuf,
    /// `through` of the newest snapshot saved by this process.
    newest: Arc<Mutex<Option<u64>>>,
}

impl SnapshotWriter {
    /// Durably replaces `snapshot.bin` with `bytes` (a snapshot through `through`); returns
    /// `false` without writing if a snapshot at least as new was already saved.
    pub fn save(&self, through: u64, bytes: &[u8]) -> io::Result<bool> {
        let mut newest = self.newest.lock().unwrap_or_else(|e| e.into_inner());
        if newest.is_some_and(|n| n >= through) {
            return Ok(false);
        }
        let tmp = self.dir.join("snapshot.tmp");
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, self.dir.join("snapshot.bin"))?;
        sync_dir(&self.dir)?;
        *newest = Some(through);
        Ok(true)
    }
}

/// Creates `dir` with any missing parents and makes the entries of `dir` and its ancestors
/// durable. Done on every open, for the same reason as the log's own sync.
fn create_dir_durably(dir: &Path) -> io::Result<()> {
    let dir = std::path::absolute(dir)?;
    let created: Vec<PathBuf> = dir.ancestors().take_while(|d| !d.exists()).map(Path::to_path_buf).collect();
    fs::create_dir_all(&dir)?;
    for parent in dir.ancestors().skip(1) {
        match sync_dir(parent) {
            // An older directory this process may not read holds no entry it just made.
            Err(e)
                if e.kind() == io::ErrorKind::PermissionDenied
                    && !created.iter().any(|c| c.parent() == Some(parent)) => {}
            r => r?,
        }
    }
    Ok(())
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

const HEADER: usize = 12;
/// The unit a crash loses data in: a lost write leaves whole sectors of zeros.
const SECTOR: usize = 512;

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

/// The records in `bytes` and the length of the valid prefix holding them; an error if
/// what follows that prefix is not a torn tail.
fn read_frames(bytes: &[u8]) -> io::Result<(Vec<Bytes>, usize)> {
    // A lost write reads back as zeros from where it started (a frame boundary) or from a
    // sector boundary within it: where the zeros ending the file start, and the first sector
    // boundary from there.
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
            || io::Error::new(io::ErrorKind::InvalidData, format!("records.log is corrupt at byte {pos}"));
        // A frame that fails its check is torn only if the lost write reaches into it.
        if crc32fast::hash(&rest[..4]) != u32_at(rest, 4) {
            if zero_run <= pos || zeros < pos + HEADER {
                break;
            }
            return Err(corrupt());
        }
        let end = HEADER + u32_at(rest, 0) as usize;
        let Some(record) = rest.get(HEADER..end) else { break };
        if crc32fast::hash(record) != u32_at(rest, 8) {
            if zeros < pos + end {
                break;
            }
            return Err(corrupt());
        }
        out.push(Bytes::copy_from_slice(record));
        pos += end;
    }
    Ok((out, pos))
}

fn frame(records: &[Bytes]) -> Vec<u8> {
    let mut buf = Vec::new();
    for r in records {
        let len = (r.len() as u32).to_le_bytes();
        buf.extend_from_slice(&len);
        buf.extend_from_slice(&crc32fast::hash(&len).to_le_bytes());
        buf.extend_from_slice(&crc32fast::hash(r).to_le_bytes());
        buf.extend_from_slice(r);
    }
    buf
}

impl Disk {
    /// Opens (creating) the data directory; returns the durable records in write order.
    pub fn open(dir: &Path) -> io::Result<(Self, Vec<Bytes>)> {
        create_dir_durably(dir)?;
        let path = dir.join("records.log");
        let bytes = match fs::read(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            r => r?,
        };
        let (records, valid) = read_frames(&bytes)?;
        if valid < bytes.len() {
            // Truncate through a writable handle: append-only handles cannot (Windows).
            let f = OpenOptions::new().write(true).open(&path)?;
            f.set_len(valid as u64)?;
            f.sync_all()?;
        }
        let log = OpenOptions::new().create(true).append(true).open(&path)?;
        // The log's directory entry must survive a crash before anything is acknowledged.
        // Synced on every open, not just when created: a crash right after creating it
        // may have left the entry only in memory.
        log.sync_all()?;
        sync_dir(dir)?;
        let snapshots = SnapshotWriter { dir: dir.to_path_buf(), newest: Arc::default() };
        Ok((Self { dir: dir.to_path_buf(), log, snapshots }, records))
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

    pub fn snapshots(&self) -> SnapshotWriter {
        self.snapshots.clone()
    }

    pub fn load_snapshot(&self) -> io::Result<Option<Bytes>> {
        match fs::read(self.dir.join("snapshot.bin")) {
            Ok(b) => Ok(Some(Bytes::from(b))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// `[through u64][core len u32][core][count u32]([klen u32][k][vlen u32][v])*[crc32 u32]`,
/// the checksum covering everything before it.
pub fn encode_snapshot(through: u64, core: &[u8], kv: &Kv) -> Vec<u8> {
    let size = 20 + core.len() + kv.iter().map(|(k, v)| 8 + k.len() + v.len()).sum::<usize>();
    let mut b = Vec::with_capacity(size);
    b.extend_from_slice(&through.to_le_bytes());
    b.extend_from_slice(&(core.len() as u32).to_le_bytes());
    b.extend_from_slice(core);
    b.extend_from_slice(&(kv.len() as u32).to_le_bytes());
    for (k, v) in kv.iter() {
        for part in [k.as_bytes(), v.as_bytes()] {
            b.extend_from_slice(&(part.len() as u32).to_le_bytes());
            b.extend_from_slice(part);
        }
    }
    b.extend_from_slice(&crc32fast::hash(&b).to_le_bytes());
    b
}

pub fn decode_snapshot(b: &Bytes) -> Option<Snapshot> {
    let body = b.len().checked_sub(4)?;
    if crc32fast::hash(&b[..body]) != u32_at(b, body) {
        return None;
    }
    let b = b.slice(..body);
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
    let mut kv = Kv::default();
    for _ in 0..count {
        let mut part = || {
            let len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            String::from_utf8(take(len)?.to_vec()).ok()
        };
        let (k, v) = (part()?, part()?);
        kv.insert(k, v);
    }
    if pos != b.len() {
        return None;
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

    fn append_raw(dir: &Path, bytes: &[u8]) {
        OpenOptions::new().append(true).open(dir.join("records.log")).unwrap().write_all(bytes).unwrap();
    }

    fn two_records(name: &str) -> PathBuf {
        let dir = temp_dir(name);
        let (mut disk, none) = Disk::open(&dir).unwrap();
        assert!(none.is_empty());
        disk.append(&[Bytes::from_static(b"a"), Bytes::from_static(b"bc")]).unwrap();
        dir
    }

    const AB: [Bytes; 2] = [Bytes::from_static(b"a"), Bytes::from_static(b"bc")];

    #[test]
    fn records_survive_reopen_and_a_torn_tail_is_dropped() {
        let dir = two_records("torn");
        let whole = frame(&[Bytes::from_static(b"ninebytes")]);
        append_raw(&dir, &whole[..HEADER + 1]); // a frame cut short by a crash
        let (_, records) = Disk::open(&dir).unwrap();
        assert_eq!(records, AB);
        let len = fs::metadata(dir.join("records.log")).unwrap().len();
        assert_eq!(len as usize, frame(&AB).len(), "the torn tail was truncated");
    }

    #[test]
    fn a_zero_filled_tail_is_dropped() {
        // A crash can leave the file extended over blocks whose data never reached disk.
        let dir = two_records("zeros");
        append_raw(&dir, &[0; 40]);
        assert_eq!(Disk::open(&dir).unwrap().1, AB);
        // Whole sectors are lost: here the record's last 1000 bytes.
        let mut torn = frame(&[Bytes::from(vec![7u8; 1100])]);
        torn[HEADER + 100..].fill(0);
        append_raw(&dir, &torn);
        assert_eq!(Disk::open(&dir).unwrap().1, AB);
    }

    #[test]
    fn a_corrupt_frame_before_the_tail_refuses_to_open() {
        for (name, at) in [("length", 0), ("body", HEADER)] {
            let dir = two_records(name);
            let path = dir.join("records.log");
            let mut bytes = fs::read(&path).unwrap();
            bytes[at] ^= 0x40; // the first record's length (pointing past the end) or payload
            fs::write(&path, &bytes).unwrap();
            let err = Disk::open(&dir).err().expect("corruption is not a torn tail");
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{name}");
            assert_eq!(fs::read(&path).unwrap(), bytes, "{name}: nothing was truncated");
        }
    }

    #[test]
    fn a_torn_sector_may_start_inside_a_frame_header() {
        // The next frame's header straddles a sector boundary; the sector after it is lost.
        let dir = two_records("straddle");
        let filler = Bytes::from(vec![5u8; SECTOR - 6 - frame(&AB).len() - HEADER]);
        Disk::open(&dir).unwrap().0.append(std::slice::from_ref(&filler)).unwrap();
        let mut torn = frame(&[Bytes::from(vec![7u8; 600])]);
        torn[6..].fill(0);
        append_raw(&dir, &torn);
        assert_eq!(Disk::open(&dir).unwrap().1, [AB[0].clone(), AB[1].clone(), filler]);
    }

    #[test]
    fn a_zeroed_sector_after_a_later_record_does_not_excuse_a_corrupt_one() {
        let dir = two_records("unrelated");
        let path = dir.join("records.log");
        let mut bytes = fs::read(&path).unwrap();
        bytes[HEADER] ^= 0x40; // the first record's payload
        bytes.extend(frame(&[Bytes::from(vec![3u8; 600])]));
        bytes.extend([0; 2 * SECTOR]);
        fs::write(&path, &bytes).unwrap();
        assert_eq!(Disk::open(&dir).err().expect("corrupt").kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn a_corrupt_final_record_refuses_to_open() {
        let dir = two_records("last");
        let path = dir.join("records.log");
        let mut bytes = fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 0x40;
        fs::write(&path, &bytes).unwrap();
        let err = Disk::open(&dir).err().expect("an acknowledged record is not a torn tail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[cfg(unix)]
    #[test]
    fn a_new_directory_whose_parent_cannot_be_synced_refuses_to_open() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir("unreadable");
        fs::create_dir_all(&root).unwrap();
        // Write and search, no read: the entry can be created but the parent not opened.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o300)).unwrap();
        let readable = fs::File::open(&root).is_ok(); // as root
        let opened = Disk::open(&root.join("data"));
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        if readable {
            return;
        }
        assert_eq!(
            opened.err().expect("the new entry cannot be made durable").kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn nested_data_directories_are_created() {
        let root = temp_dir("nested");
        let (mut disk, _) = Disk::open(&root.join("a/b")).unwrap();
        disk.append(&[Bytes::from_static(b"x")]).unwrap();
        assert_eq!(Disk::open(&root.join("a/b")).unwrap().1, [Bytes::from_static(b"x")]);
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
        let kv = Kv::from_iter([("k".to_owned(), "v".to_owned())]);
        let s = Snapshot { through: 7, core: Bytes::from_static(b"core"), kv };
        let d = decode_snapshot(&Bytes::from(encode_snapshot(s.through, &s.core, &s.kv))).unwrap();
        assert_eq!((d.through, d.core, d.kv), (s.through, s.core, s.kv));
    }

    #[test]
    fn a_damaged_snapshot_is_rejected() {
        let kv = Kv::from_iter([("k".to_owned(), "v".to_owned())]);
        let good = encode_snapshot(7, b"core", &kv);
        for i in 0..good.len() {
            let mut bad = good.clone();
            bad[i] ^= 1;
            assert!(decode_snapshot(&Bytes::from(bad)).is_none(), "flipped byte {i}");
        }
        let mut longer = good.clone();
        longer.push(0);
        assert!(decode_snapshot(&Bytes::from(longer)).is_none());
        assert!(decode_snapshot(&Bytes::from(good[..good.len() - 1].to_vec())).is_none());
    }

    #[test]
    fn an_older_snapshot_never_replaces_a_newer_one() {
        let dir = temp_dir("snapshots");
        let (disk, _) = Disk::open(&dir).unwrap();
        let w = disk.snapshots();
        assert!(w.save(9, b"nine").unwrap());
        assert!(!w.save(5, b"five").unwrap());
        assert!(!w.save(9, b"nine again").unwrap());
        assert_eq!(disk.load_snapshot().unwrap().as_deref(), Some(&b"nine"[..]));
    }
}

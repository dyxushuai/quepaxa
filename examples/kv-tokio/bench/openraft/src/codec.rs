//! Zero-dependency binary framing for Raft WAL records, `StreamAppend` payloads,
//! snapshots, and forwarded KV commands so byte payloads avoid JSON integer-array blowup.

use std::io;

use openraft::EntryPayload;
use openraft::raft::AppendEntriesRequest;
use openraft::vote::RaftLeaderId;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::types::{
    CmpOp, CmpTarget, CommittedLeaderId, Entry, KvEntry, KvOpResult, KvRequest, KvResponse, KvResponseKind,
    LogId, RangeQuery, SnapshotMeta, StoredMembership, TxnCompare, TxnOp, TypeConfig, Vote,
};

pub enum WalRecord {
    Vote(Vote),
    Append(Vec<Entry>),
    Truncate(Option<LogId>),
    Purge(LogId),
}

pub struct SnapshotPayload {
    pub last_applied: Option<LogId>,
    pub last_membership: StoredMembership,
    pub revision: u64,
    pub kvs: Vec<(Vec<u8>, KvEntry)>,
}

pub struct SnapshotWire {
    pub vote: Vote,
    pub meta: SnapshotMeta,
    pub data: Vec<u8>,
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_len(buf: &mut Vec<u8>, len: usize) -> io::Result<()> {
    let n = u32::try_from(len).map_err(|_| bad("length exceeds u32"))?;
    put_u32(buf, n);
    Ok(())
}

fn put_bytes(buf: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    put_len(buf, bytes.len())?;
    buf.extend_from_slice(bytes);
    Ok(())
}

fn put_json<T: Serialize>(buf: &mut Vec<u8>, value: &T) -> io::Result<()> {
    let encoded = serde_json::to_vec(value).map_err(|e| bad(e.to_string()))?;
    put_bytes(buf, &encoded)
}

fn put_log_id(buf: &mut Vec<u8>, id: &LogId) {
    put_u64(buf, id.leader_id.term());
    put_u64(buf, *id.leader_id.node_id());
    put_u64(buf, id.index);
}

fn put_opt_log_id(buf: &mut Vec<u8>, id: Option<&LogId>) {
    match id {
        None => buf.push(0),
        Some(id) => {
            buf.push(1);
            put_log_id(buf, id);
        }
    }
}

fn put_kv_entry(buf: &mut Vec<u8>, e: &KvEntry) -> io::Result<()> {
    put_bytes(buf, &e.value)?;
    put_u64(buf, e.create_revision);
    put_u64(buf, e.mod_revision);
    put_u64(buf, e.version);
    Ok(())
}

fn put_range_query(buf: &mut Vec<u8>, q: &RangeQuery) -> io::Result<()> {
    put_bytes(buf, &q.key)?;
    put_bytes(buf, &q.range_end)?;
    put_u64(buf, q.limit);
    buf.push(u8::from(q.count_only) | (u8::from(q.keys_only) << 1));
    Ok(())
}

fn put_txn_op(buf: &mut Vec<u8>, op: &TxnOp) -> io::Result<()> {
    match op {
        TxnOp::Range(q) => {
            buf.push(0);
            put_range_query(buf, q)
        }
        TxnOp::Put { key, value, prev_kv } => {
            buf.push(1);
            put_bytes(buf, key)?;
            put_bytes(buf, value)?;
            buf.push(u8::from(*prev_kv));
            Ok(())
        }
        TxnOp::Delete { key, range_end, prev_kv } => {
            buf.push(2);
            put_bytes(buf, key)?;
            put_bytes(buf, range_end)?;
            buf.push(u8::from(*prev_kv));
            Ok(())
        }
    }
}

fn put_kv_request(buf: &mut Vec<u8>, req: &KvRequest) -> io::Result<()> {
    match req {
        KvRequest::Put { key, value, prev_kv } => {
            buf.push(0);
            put_bytes(buf, key)?;
            put_bytes(buf, value)?;
            buf.push(u8::from(*prev_kv));
            Ok(())
        }
        KvRequest::DeleteRange { key, range_end, prev_kv } => {
            buf.push(1);
            put_bytes(buf, key)?;
            put_bytes(buf, range_end)?;
            buf.push(u8::from(*prev_kv));
            Ok(())
        }
        KvRequest::Txn { compares, success, failure } => {
            buf.push(2);
            put_len(buf, compares.len())?;
            for c in compares {
                put_bytes(buf, &c.key)?;
                buf.push(match c.op {
                    CmpOp::Equal => 0,
                    CmpOp::NotEqual => 1,
                    CmpOp::Less => 2,
                    CmpOp::Greater => 3,
                });
                match &c.target {
                    CmpTarget::Value(v) => {
                        buf.push(0);
                        put_bytes(buf, v)?;
                    }
                    CmpTarget::Version(n) => {
                        buf.push(1);
                        put_u64(buf, *n);
                    }
                    CmpTarget::Create(n) => {
                        buf.push(2);
                        put_u64(buf, *n);
                    }
                    CmpTarget::Mod(n) => {
                        buf.push(3);
                        put_u64(buf, *n);
                    }
                }
            }
            for branch in [success, failure] {
                put_len(buf, branch.len())?;
                for op in branch {
                    put_txn_op(buf, op)?;
                }
            }
            Ok(())
        }
    }
}

fn put_entry(buf: &mut Vec<u8>, entry: &Entry) -> io::Result<()> {
    put_log_id(buf, &entry.log_id);
    match &entry.payload {
        EntryPayload::Blank => {
            buf.push(0);
            Ok(())
        }
        EntryPayload::Normal(req) => {
            buf.push(1);
            put_kv_request(buf, req)
        }
        EntryPayload::Membership(mem) => {
            buf.push(2);
            put_json(buf, mem)
        }
    }
}

fn put_kvs(buf: &mut Vec<u8>, kvs: &[(Vec<u8>, KvEntry)]) -> io::Result<()> {
    put_len(buf, kvs.len())?;
    for (k, v) in kvs {
        put_bytes(buf, k)?;
        put_kv_entry(buf, v)?;
    }
    Ok(())
}

fn put_opt_kv(buf: &mut Vec<u8>, kv: &Option<(Vec<u8>, KvEntry)>) -> io::Result<()> {
    match kv {
        None => {
            buf.push(0);
            Ok(())
        }
        Some((k, v)) => {
            buf.push(1);
            put_bytes(buf, k)?;
            put_kv_entry(buf, v)
        }
    }
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, pos: 0 }
    }

    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(|| bad("length overflow"))?;
        let slice = self.b.get(self.pos..end).ok_or_else(|| bad("truncated binary payload"))?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn flag(&mut self) -> io::Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(bad(format!("invalid bool tag {other}"))),
        }
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }

    fn json<T: DeserializeOwned>(&mut self) -> io::Result<T> {
        let n = self.u32()? as usize;
        let raw = self.take(n)?;
        serde_json::from_slice(raw).map_err(|e| bad(e.to_string()))
    }

    fn log_id(&mut self) -> io::Result<LogId> {
        let term = self.u64()?;
        let node_id = self.u64()?;
        let index = self.u64()?;
        Ok(LogId::new(CommittedLeaderId::new(term, node_id), index))
    }

    fn opt_log_id(&mut self) -> io::Result<Option<LogId>> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.log_id()?)),
            other => Err(bad(format!("invalid Option<LogId> tag {other}"))),
        }
    }

    fn kv_entry(&mut self) -> io::Result<KvEntry> {
        Ok(KvEntry {
            value: self.bytes()?,
            create_revision: self.u64()?,
            mod_revision: self.u64()?,
            version: self.u64()?,
        })
    }

    fn range_query(&mut self) -> io::Result<RangeQuery> {
        let key = self.bytes()?;
        let range_end = self.bytes()?;
        let limit = self.u64()?;
        let flags = self.u8()?;
        Ok(RangeQuery { key, range_end, limit, count_only: flags & 1 != 0, keys_only: flags & 2 != 0 })
    }

    fn txn_op(&mut self) -> io::Result<TxnOp> {
        match self.u8()? {
            0 => Ok(TxnOp::Range(self.range_query()?)),
            1 => Ok(TxnOp::Put { key: self.bytes()?, value: self.bytes()?, prev_kv: self.flag()? }),
            2 => Ok(TxnOp::Delete { key: self.bytes()?, range_end: self.bytes()?, prev_kv: self.flag()? }),
            other => Err(bad(format!("invalid TxnOp tag {other}"))),
        }
    }

    fn kv_request(&mut self) -> io::Result<KvRequest> {
        match self.u8()? {
            0 => Ok(KvRequest::Put { key: self.bytes()?, value: self.bytes()?, prev_kv: self.flag()? }),
            1 => Ok(KvRequest::DeleteRange {
                key: self.bytes()?,
                range_end: self.bytes()?,
                prev_kv: self.flag()?,
            }),
            2 => {
                let cmp_len = self.u32()? as usize;
                let mut compares = Vec::with_capacity(cmp_len.min(1024));
                for _ in 0..cmp_len {
                    let key = self.bytes()?;
                    let op = match self.u8()? {
                        0 => CmpOp::Equal,
                        1 => CmpOp::NotEqual,
                        2 => CmpOp::Less,
                        3 => CmpOp::Greater,
                        other => return Err(bad(format!("invalid CmpOp tag {other}"))),
                    };
                    let target = match self.u8()? {
                        0 => CmpTarget::Value(self.bytes()?),
                        1 => CmpTarget::Version(self.u64()?),
                        2 => CmpTarget::Create(self.u64()?),
                        3 => CmpTarget::Mod(self.u64()?),
                        other => return Err(bad(format!("invalid CmpTarget tag {other}"))),
                    };
                    compares.push(TxnCompare { key, op, target });
                }
                let ok_len = self.u32()? as usize;
                let mut success = Vec::with_capacity(ok_len.min(1024));
                for _ in 0..ok_len {
                    success.push(self.txn_op()?);
                }
                let fail_len = self.u32()? as usize;
                let mut failure = Vec::with_capacity(fail_len.min(1024));
                for _ in 0..fail_len {
                    failure.push(self.txn_op()?);
                }
                Ok(KvRequest::Txn { compares, success, failure })
            }
            other => Err(bad(format!("invalid KvRequest tag {other}"))),
        }
    }

    fn entry(&mut self) -> io::Result<Entry> {
        let log_id = self.log_id()?;
        let payload = match self.u8()? {
            0 => EntryPayload::Blank,
            1 => EntryPayload::Normal(self.kv_request()?),
            2 => EntryPayload::Membership(self.json()?),
            other => return Err(bad(format!("invalid EntryPayload tag {other}"))),
        };
        Ok(Entry { log_id, payload })
    }

    fn kvs(&mut self) -> io::Result<Vec<(Vec<u8>, KvEntry)>> {
        let n = self.u32()? as usize;
        let mut out = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            out.push((self.bytes()?, self.kv_entry()?));
        }
        Ok(out)
    }

    fn opt_kv(&mut self) -> io::Result<Option<(Vec<u8>, KvEntry)>> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some((self.bytes()?, self.kv_entry()?))),
            other => Err(bad(format!("invalid Option<KV> tag {other}"))),
        }
    }

    fn finish(self) -> io::Result<()> {
        if self.pos == self.b.len() { Ok(()) } else { Err(bad("trailing bytes in binary payload")) }
    }
}

pub fn encode_kv_request(req: &KvRequest) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(req.approx_bytes());
    put_kv_request(&mut buf, req)?;
    Ok(buf)
}

pub fn decode_kv_request(bytes: &[u8]) -> io::Result<KvRequest> {
    let mut r = Reader::new(bytes);
    let req = r.kv_request()?;
    r.finish()?;
    Ok(req)
}

pub fn encode_kv_response(resp: &KvResponse) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(64);
    put_u64(&mut buf, resp.revision);
    match &resp.kind {
        KvResponseKind::Put { prev } => {
            buf.push(0);
            put_opt_kv(&mut buf, prev)?;
        }
        KvResponseKind::Delete { deleted, prev } => {
            buf.push(1);
            put_u64(&mut buf, *deleted);
            put_kvs(&mut buf, prev)?;
        }
        KvResponseKind::Txn { succeeded, responses } => {
            buf.push(2);
            buf.push(u8::from(*succeeded));
            put_len(&mut buf, responses.len())?;
            for r in responses {
                match r {
                    KvOpResult::Range { kvs, more, count } => {
                        buf.push(0);
                        put_kvs(&mut buf, kvs)?;
                        buf.push(u8::from(*more));
                        put_u64(&mut buf, *count);
                    }
                    KvOpResult::Put { prev } => {
                        buf.push(1);
                        put_opt_kv(&mut buf, prev)?;
                    }
                    KvOpResult::Delete { deleted, prev } => {
                        buf.push(2);
                        put_u64(&mut buf, *deleted);
                        put_kvs(&mut buf, prev)?;
                    }
                }
            }
        }
    }
    Ok(buf)
}

pub fn decode_kv_response(bytes: &[u8]) -> io::Result<KvResponse> {
    let mut r = Reader::new(bytes);
    let revision = r.u64()?;
    let kind = match r.u8()? {
        0 => KvResponseKind::Put { prev: r.opt_kv()? },
        1 => KvResponseKind::Delete { deleted: r.u64()?, prev: r.kvs()? },
        2 => {
            let succeeded = r.flag()?;
            let n = r.u32()? as usize;
            let mut responses = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                let op = match r.u8()? {
                    0 => KvOpResult::Range { kvs: r.kvs()?, more: r.flag()?, count: r.u64()? },
                    1 => KvOpResult::Put { prev: r.opt_kv()? },
                    2 => KvOpResult::Delete { deleted: r.u64()?, prev: r.kvs()? },
                    other => return Err(bad(format!("invalid KvOpResult tag {other}"))),
                };
                responses.push(op);
            }
            KvResponseKind::Txn { succeeded, responses }
        }
        other => return Err(bad(format!("invalid KvResponseKind tag {other}"))),
    };
    r.finish()?;
    Ok(KvResponse { revision, kind })
}

pub fn encode_wal_vote(vote: &Vote) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(64);
    buf.push(0);
    put_json(&mut buf, vote)?;
    Ok(buf)
}

pub fn encode_wal_append(entries: &[Entry]) -> io::Result<Vec<u8>> {
    let cap: usize = entries
        .iter()
        .map(|e| match &e.payload {
            EntryPayload::Normal(r) => 28 + r.approx_bytes(),
            _ => 64,
        })
        .sum();
    let mut buf = Vec::with_capacity(8 + cap);
    buf.push(1);
    put_len(&mut buf, entries.len())?;
    for e in entries {
        put_entry(&mut buf, e)?;
    }
    Ok(buf)
}

pub fn encode_wal_truncate(last: Option<LogId>) -> Vec<u8> {
    let mut buf = Vec::with_capacity(26);
    buf.push(2);
    put_opt_log_id(&mut buf, last.as_ref());
    buf
}

pub fn encode_wal_purge(id: LogId) -> Vec<u8> {
    let mut buf = Vec::with_capacity(25);
    buf.push(3);
    put_log_id(&mut buf, &id);
    buf
}

pub fn decode_wal_record(bytes: &[u8]) -> io::Result<WalRecord> {
    let mut r = Reader::new(bytes);
    let rec = match r.u8()? {
        0 => WalRecord::Vote(r.json()?),
        1 => {
            let n = r.u32()? as usize;
            let mut entries = Vec::with_capacity(n.min(4096));
            for _ in 0..n {
                entries.push(r.entry()?);
            }
            WalRecord::Append(entries)
        }
        2 => WalRecord::Truncate(r.opt_log_id()?),
        3 => WalRecord::Purge(r.log_id()?),
        other => return Err(bad(format!("invalid WalRecord tag {other}"))),
    };
    r.finish()?;
    Ok(rec)
}

pub fn encode_append_entries_req(req: &AppendEntriesRequest<TypeConfig>) -> io::Result<Vec<u8>> {
    let cap: usize = req
        .entries
        .iter()
        .map(|e| match &e.payload {
            EntryPayload::Normal(r) => 28 + r.approx_bytes(),
            _ => 64,
        })
        .sum();
    let mut buf = Vec::with_capacity(128 + cap);
    put_json(&mut buf, &req.vote)?;
    put_opt_log_id(&mut buf, req.prev_log_id.as_ref());
    put_opt_log_id(&mut buf, req.leader_commit.as_ref());
    put_len(&mut buf, req.entries.len())?;
    for e in &req.entries {
        put_entry(&mut buf, e)?;
    }
    Ok(buf)
}

pub fn decode_append_entries_req(bytes: &[u8]) -> io::Result<AppendEntriesRequest<TypeConfig>> {
    let mut r = Reader::new(bytes);
    let vote = r.json()?;
    let prev_log_id = r.opt_log_id()?;
    let leader_commit = r.opt_log_id()?;
    let n = r.u32()? as usize;
    let mut entries = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        entries.push(r.entry()?);
    }
    r.finish()?;
    Ok(AppendEntriesRequest { vote, prev_log_id, entries, leader_commit })
}

pub fn encode_snapshot_payload(payload: &SnapshotPayload) -> io::Result<Vec<u8>> {
    let cap: usize = payload.kvs.iter().map(|(k, v)| 32 + k.len() + v.value.len()).sum();
    let mut buf = Vec::with_capacity(128 + cap);
    put_opt_log_id(&mut buf, payload.last_applied.as_ref());
    put_json(&mut buf, &payload.last_membership)?;
    put_u64(&mut buf, payload.revision);
    put_kvs(&mut buf, &payload.kvs)?;
    Ok(buf)
}

pub fn decode_snapshot_payload(bytes: &[u8]) -> io::Result<SnapshotPayload> {
    let mut r = Reader::new(bytes);
    let last_applied = r.opt_log_id()?;
    let last_membership = r.json()?;
    let revision = r.u64()?;
    let kvs = r.kvs()?;
    r.finish()?;
    Ok(SnapshotPayload { last_applied, last_membership, revision, kvs })
}

pub fn encode_disk_snapshot(meta: &SnapshotMeta, data: &[u8]) -> io::Result<Vec<u8>> {
    let mut body = Vec::with_capacity(128 + data.len());
    put_json(&mut body, meta)?;
    put_bytes(&mut body, data)?;
    let crc = crc32fast::hash(&body).to_le_bytes();
    body.extend_from_slice(&crc);
    Ok(body)
}

pub fn decode_disk_snapshot(bytes: &[u8]) -> io::Result<(SnapshotMeta, Vec<u8>)> {
    let body_len = bytes.len().checked_sub(4).ok_or_else(|| bad("snapshot too short"))?;
    let (body, crc_bytes) = bytes.split_at(body_len);
    let want = u32::from_le_bytes(crc_bytes.try_into().expect("4 bytes"));
    if crc32fast::hash(body) != want {
        return Err(bad("snapshot checksum mismatch"));
    }
    let mut r = Reader::new(body);
    let meta = r.json()?;
    let data = r.bytes()?;
    r.finish()?;
    Ok((meta, data))
}

pub fn encode_snapshot_wire(vote: &Vote, meta: &SnapshotMeta, data: &[u8]) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(192 + data.len());
    put_json(&mut buf, vote)?;
    put_json(&mut buf, meta)?;
    put_bytes(&mut buf, data)?;
    Ok(buf)
}

pub fn decode_snapshot_wire(bytes: &[u8]) -> io::Result<SnapshotWire> {
    let mut r = Reader::new(bytes);
    let vote = r.json()?;
    let meta = r.json()?;
    let data = r.bytes()?;
    r.finish()?;
    Ok(SnapshotWire { vote, meta, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_codec_avoids_json_blowup_on_4kib_payloads() {
        let req = KvRequest::Put { key: b"k0000001".to_vec(), value: vec![0xAB; 4096], prev_kv: false };
        let entry = Entry {
            log_id: LogId::new(CommittedLeaderId::new(3, 1), 42),
            payload: EntryPayload::Normal(req.clone()),
        };
        let wal = encode_wal_append(std::slice::from_ref(&entry)).expect("encode wal");
        // 4 KiB value + key + fixed framing should stay well under 4.2 KiB (JSON was ~16 KiB).
        assert!(wal.len() < 4200, "unexpected WAL size: {} bytes", wal.len());
        match decode_wal_record(&wal).expect("decode wal") {
            WalRecord::Append(entries) => assert_eq!(entries, vec![entry.clone()]),
            _ => panic!("expected Append"),
        }

        let append_req = AppendEntriesRequest {
            vote: Vote::new(3, 1),
            prev_log_id: Some(LogId::new(CommittedLeaderId::new(3, 1), 41)),
            entries: vec![entry],
            leader_commit: Some(LogId::new(CommittedLeaderId::new(3, 1), 40)),
        };
        let wire = encode_append_entries_req(&append_req).expect("encode append");
        assert!(wire.len() < 4300, "unexpected wire size: {} bytes", wire.len());
        let decoded = decode_append_entries_req(&wire).expect("decode append");
        assert_eq!(decoded.entries, append_req.entries);
        assert_eq!(decoded.prev_log_id, append_req.prev_log_id);
        assert_eq!(decoded.leader_commit, append_req.leader_commit);

        let snap_payload = SnapshotPayload {
            last_applied: Some(LogId::new(CommittedLeaderId::new(3, 1), 42)),
            last_membership: StoredMembership::default(),
            revision: 99,
            kvs: vec![(
                b"big".to_vec(),
                KvEntry { value: vec![0xCD; 4096], create_revision: 1, mod_revision: 99, version: 2 },
            )],
        };
        let snap_bytes = encode_snapshot_payload(&snap_payload).expect("encode snap payload");
        let meta = SnapshotMeta {
            last_log_id: snap_payload.last_applied,
            last_membership: StoredMembership::default(),
        };
        let disk = encode_disk_snapshot(&meta, &snap_bytes).expect("encode disk snap");
        assert!(disk.len() < 4500, "unexpected disk snapshot size: {} bytes", disk.len());
        let (dec_meta, dec_data) = decode_disk_snapshot(&disk).expect("decode disk snap");
        assert_eq!(dec_meta.last_log_id, meta.last_log_id);
        let dec_payload = decode_snapshot_payload(&dec_data).expect("decode snap payload");
        assert_eq!(dec_payload.revision, 99);
        assert_eq!(dec_payload.kvs, snap_payload.kvs);
    }

    #[test]
    fn binary_codec_round_trips_txn_and_delete() {
        let txn = KvRequest::Txn {
            compares: vec![TxnCompare {
                key: b"k".to_vec(),
                op: CmpOp::Equal,
                target: CmpTarget::Version(0),
            }],
            success: vec![TxnOp::Put { key: b"k".to_vec(), value: b"v".to_vec(), prev_kv: true }],
            failure: vec![TxnOp::Delete { key: b"a".to_vec(), range_end: b"z".to_vec(), prev_kv: false }],
        };
        let encoded = encode_kv_request(&txn).expect("encode txn");
        assert_eq!(decode_kv_request(&encoded).expect("decode txn"), txn);

        let resp = KvResponse {
            revision: 7,
            kind: KvResponseKind::Txn {
                succeeded: true,
                responses: vec![KvOpResult::Put {
                    prev: Some((
                        b"k".to_vec(),
                        KvEntry { value: b"old".to_vec(), create_revision: 1, mod_revision: 2, version: 1 },
                    )),
                }],
            },
        };
        let enc_resp = encode_kv_response(&resp).expect("encode resp");
        assert_eq!(decode_kv_response(&enc_resp).expect("decode resp"), resp);
    }
}

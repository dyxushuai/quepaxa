//! Requests that go into the log, etcd-shaped: put, delete range, range (inside a
//! transaction) and single-level transactions. Every replica applies a committed request
//! the same way, so results and revisions agree everywhere.
//!
//! Encoding: `[tag u8]` then the request's fields; byte strings are `[len u32][bytes]`,
//! little endian. A transaction's branches hold no transaction.

use std::sync::Arc;

use crate::kv::{Entry, Kv};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeReq {
    pub key: Vec<u8>,
    /// See [`crate::kv::in_range`].
    pub end: Vec<u8>,
    /// At most this many keys (0: no limit).
    pub limit: u64,
    pub count_only: bool,
    pub keys_only: bool,
}

impl RangeReq {
    /// One key, its value included.
    pub fn key(key: &[u8]) -> Self {
        Self { key: key.to_vec(), end: Vec::new(), limit: 0, count_only: false, keys_only: false }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Value(Vec<u8>),
    Version(u64),
    Create(u64),
    Mod(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    Equal,
    NotEqual,
    Less,
    Greater,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compare {
    pub key: Vec<u8>,
    pub cmp: Cmp,
    pub target: Target,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Range(RangeReq),
    Put { key: Vec<u8>, value: Vec<u8>, prev_kv: bool },
    Delete { key: Vec<u8>, end: Vec<u8>, prev_kv: bool },
    Txn { compares: Vec<Compare>, success: Vec<Request>, failure: Vec<Request> },
}

pub type Kvs = Vec<(Arc<[u8]>, Entry)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Range { kvs: Kvs, more: bool, count: u64 },
    Put { prev: Option<Entry> },
    Delete { deleted: u64, prev: Kvs },
    Txn { succeeded: bool, responses: Vec<Response> },
}

/// A request's result and the store revision after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub revision: u64,
    pub response: Response,
}

const RANGE: u8 = 1;
const PUT: u8 = 2;
const DELETE: u8 = 3;
const TXN: u8 = 4;

fn put_bytes(b: &mut Vec<u8>, s: &[u8]) {
    b.extend_from_slice(&(s.len() as u32).to_le_bytes());
    b.extend_from_slice(s);
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        self.encode_into(&mut b);
        b
    }

    fn encode_into(&self, b: &mut Vec<u8>) {
        match self {
            Request::Range(r) => {
                b.push(RANGE);
                put_bytes(b, &r.key);
                put_bytes(b, &r.end);
                b.extend_from_slice(&r.limit.to_le_bytes());
                b.push(u8::from(r.count_only) | u8::from(r.keys_only) << 1);
            }
            Request::Put { key, value, prev_kv } => {
                b.push(PUT);
                put_bytes(b, key);
                put_bytes(b, value);
                b.push(u8::from(*prev_kv));
            }
            Request::Delete { key, end, prev_kv } => {
                b.push(DELETE);
                put_bytes(b, key);
                put_bytes(b, end);
                b.push(u8::from(*prev_kv));
            }
            Request::Txn { compares, success, failure } => {
                b.push(TXN);
                b.extend_from_slice(&(compares.len() as u32).to_le_bytes());
                for c in compares {
                    put_bytes(b, &c.key);
                    b.push(c.cmp as u8);
                    match &c.target {
                        Target::Value(v) => {
                            b.push(0);
                            put_bytes(b, v);
                        }
                        Target::Version(n) | Target::Create(n) | Target::Mod(n) => {
                            b.push(match c.target {
                                Target::Version(_) => 1,
                                Target::Create(_) => 2,
                                _ => 3,
                            });
                            b.extend_from_slice(&n.to_le_bytes());
                        }
                    }
                }
                for branch in [success, failure] {
                    b.extend_from_slice(&(branch.len() as u32).to_le_bytes());
                    for r in branch {
                        r.encode_into(b);
                    }
                }
            }
        }
    }

    /// `None` for anything that is not a well-formed request, including a transaction
    /// inside a transaction.
    pub fn decode(b: &[u8]) -> Option<Self> {
        let mut r = Reader { b, pos: 0 };
        let req = r.request(true)?;
        (r.pos == b.len()).then_some(req)
    }
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.b.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(s)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()? as usize;
        Some(self.take(n)?.to_vec())
    }

    fn flag(&mut self) -> Option<bool> {
        match self.u8()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }

    fn request(&mut self, txn_allowed: bool) -> Option<Request> {
        Some(match self.u8()? {
            RANGE => {
                let (key, end, limit, flags) = (self.bytes()?, self.bytes()?, self.u64()?, self.u8()?);
                if flags > 3 {
                    return None;
                }
                Request::Range(RangeReq {
                    key,
                    end,
                    limit,
                    count_only: flags & 1 != 0,
                    keys_only: flags & 2 != 0,
                })
            }
            PUT => Request::Put { key: self.bytes()?, value: self.bytes()?, prev_kv: self.flag()? },
            DELETE => Request::Delete { key: self.bytes()?, end: self.bytes()?, prev_kv: self.flag()? },
            TXN if txn_allowed => {
                let n = self.u32()?;
                let mut compares = Vec::new();
                for _ in 0..n {
                    let key = self.bytes()?;
                    let cmp = match self.u8()? {
                        0 => Cmp::Equal,
                        1 => Cmp::NotEqual,
                        2 => Cmp::Less,
                        3 => Cmp::Greater,
                        _ => return None,
                    };
                    let target = match self.u8()? {
                        0 => Target::Value(self.bytes()?),
                        1 => Target::Version(self.u64()?),
                        2 => Target::Create(self.u64()?),
                        3 => Target::Mod(self.u64()?),
                        _ => return None,
                    };
                    compares.push(Compare { key, cmp, target });
                }
                let mut branch = || -> Option<Vec<Request>> {
                    let n = self.u32()?;
                    (0..n).map(|_| self.request(false)).collect()
                };
                let (success, failure) = (branch()?, branch()?);
                Request::Txn { compares, success, failure }
            }
            _ => return None,
        })
    }
}

fn order<T: Ord>(a: T, b: T, cmp: Cmp) -> bool {
    match cmp {
        Cmp::Equal => a == b,
        Cmp::NotEqual => a != b,
        Cmp::Less => a < b,
        Cmp::Greater => a > b,
    }
}

/// etcd's rule: a missing key compares as version, create and mod revision 0, and every
/// value comparison on it fails.
fn holds(kv: &Kv, c: &Compare) -> bool {
    let e = kv.get(&c.key);
    match &c.target {
        Target::Value(v) => e.is_some_and(|e| order(&*e.value, &v[..], c.cmp)),
        Target::Version(n) => order(e.map_or(0, |e| e.version), *n, c.cmp),
        Target::Create(n) => order(e.map_or(0, |e| e.create), *n, c.cmp),
        Target::Mod(n) => order(e.map_or(0, |e| e.modified), *n, c.cmp),
    }
}

fn range(kv: &Kv, r: &RangeReq) -> Response {
    let mut kvs = kv.range(&r.key, &r.end);
    let count = kvs.len() as u64;
    let more = r.limit > 0 && count > r.limit;
    if r.count_only {
        kvs.clear();
    } else {
        if more {
            kvs.truncate(r.limit as usize);
        }
        if r.keys_only {
            for (_, e) in &mut kvs {
                e.value = Arc::from(&[][..]);
            }
        }
    }
    Response::Range { kvs, more, count }
}

/// A range against the current state, without a write.
pub fn read(kv: &Kv, r: &RangeReq) -> Applied {
    Applied { revision: kv.revision(), response: range(kv, r) }
}

/// Applies `req`; a request that writes anything takes one new revision for all its writes.
pub fn apply(kv: &mut Kv, req: &Request) -> Applied {
    let mut rev = None;
    let response = run(kv, req, &mut rev);
    Applied { revision: kv.revision(), response }
}

fn run(kv: &mut Kv, req: &Request, rev: &mut Option<u64>) -> Response {
    let mut revision = |kv: &mut Kv| *rev.get_or_insert_with(|| kv.next_revision());
    match req {
        Request::Range(r) => range(kv, r),
        Request::Put { key, value, prev_kv } => {
            let at = revision(kv);
            let prev = kv.put(key, value, at);
            Response::Put { prev: prev.filter(|_| *prev_kv) }
        }
        Request::Delete { key, end, prev_kv } => {
            let doomed = kv.range(key, end);
            if !doomed.is_empty() {
                revision(kv);
            }
            for (k, _) in &doomed {
                kv.delete(k);
            }
            let deleted = doomed.len() as u64;
            Response::Delete { deleted, prev: if *prev_kv { doomed } else { Vec::new() } }
        }
        Request::Txn { compares, success, failure } => {
            let succeeded = compares.iter().all(|c| holds(kv, c));
            let branch = if succeeded { success } else { failure };
            let responses = branch.iter().map(|r| run(kv, r, rev)).collect();
            Response::Txn { succeeded, responses }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(k: &str, v: &str) -> Request {
        Request::Put { key: k.into(), value: v.into(), prev_kv: true }
    }

    fn get(k: &str) -> Request {
        Request::Range(RangeReq {
            key: k.into(),
            end: Vec::new(),
            limit: 0,
            count_only: false,
            keys_only: false,
        })
    }

    fn cas(k: &str, modified: u64, v: &str) -> Request {
        Request::Txn {
            compares: vec![Compare { key: k.into(), cmp: Cmp::Equal, target: Target::Mod(modified) }],
            success: vec![put(k, v)],
            failure: vec![get(k)],
        }
    }

    #[test]
    fn puts_take_a_revision_each_and_report_the_previous_entry() {
        let mut kv = Kv::default();
        let a = apply(&mut kv, &put("a", "1"));
        assert_eq!(a, Applied { revision: 1, response: Response::Put { prev: None } });
        let b = apply(&mut kv, &put("a", "2"));
        let Response::Put { prev: Some(prev) } = b.response else { panic!("{b:?}") };
        assert_eq!((b.revision, &*prev.value, prev.modified), (2, &b"1"[..], 1));
        let e = kv.get(b"a").unwrap();
        assert_eq!((e.create, e.modified, e.version), (1, 2, 2));
    }

    #[test]
    fn a_delete_range_takes_one_revision_and_a_recreated_key_starts_over() {
        let mut kv = Kv::default();
        for k in ["k1", "k2", "k3", "l"] {
            apply(&mut kv, &put(k, "v"));
        }
        let d = apply(&mut kv, &Request::Delete { key: b"k".to_vec(), end: b"l".to_vec(), prev_kv: false });
        assert_eq!(d, Applied { revision: 5, response: Response::Delete { deleted: 3, prev: Vec::new() } });
        let none =
            apply(&mut kv, &Request::Delete { key: b"k".to_vec(), end: b"l".to_vec(), prev_kv: false });
        assert_eq!(none.revision, 5, "deleting nothing is not a write");
        apply(&mut kv, &put("k2", "again"));
        let e = kv.get(b"k2").unwrap();
        assert_eq!((e.create, e.version), (6, 1));
    }

    #[test]
    fn compare_and_swap_on_mod_revision_succeeds_once() {
        let mut kv = Kv::default();
        apply(&mut kv, &put("lock", "free"));
        let first = apply(&mut kv, &cas("lock", 1, "mine"));
        assert!(matches!(first.response, Response::Txn { succeeded: true, .. }));
        assert_eq!(first.revision, 2);
        let retry = apply(&mut kv, &cas("lock", 1, "theirs"));
        let Response::Txn { succeeded: false, responses } = retry.response else { panic!("{retry:?}") };
        let Response::Range { kvs, .. } = &responses[0] else { panic!() };
        assert_eq!(&*kvs[0].1.value, b"mine");
        assert_eq!(retry.revision, 2, "a transaction that only reads takes no revision");
    }

    #[test]
    fn a_missing_key_compares_as_zero_and_fails_value_comparisons() {
        let mut kv = Kv::default();
        let create_if_absent = |v: &str| Request::Txn {
            compares: vec![Compare { key: b"x".to_vec(), cmp: Cmp::Equal, target: Target::Create(0) }],
            success: vec![put("x", v)],
            failure: Vec::new(),
        };
        assert!(matches!(
            apply(&mut kv, &create_if_absent("1")).response,
            Response::Txn { succeeded: true, .. }
        ));
        assert!(matches!(
            apply(&mut kv, &create_if_absent("2")).response,
            Response::Txn { succeeded: false, .. }
        ));
        let on_missing = Request::Txn {
            compares: vec![Compare {
                key: b"y".to_vec(),
                cmp: Cmp::NotEqual,
                target: Target::Value(b"z".to_vec()),
            }],
            success: Vec::new(),
            failure: Vec::new(),
        };
        assert!(matches!(apply(&mut kv, &on_missing).response, Response::Txn { succeeded: false, .. }));
    }

    #[test]
    fn ranges_honour_limit_count_only_and_keys_only() {
        let mut kv = Kv::default();
        for k in ["a", "b", "c"] {
            apply(&mut kv, &put(k, "value"));
        }
        let r = |limit, count_only, keys_only| {
            let req = RangeReq { key: b"a".to_vec(), end: b"\0".to_vec(), limit, count_only, keys_only };
            apply(&mut kv.clone(), &Request::Range(req)).response
        };
        let Response::Range { kvs, more, count } = r(2, false, false) else { panic!() };
        assert_eq!((kvs.len(), more, count, &*kvs[1].0), (2, true, 3, &b"b"[..]));
        let Response::Range { kvs, count, .. } = r(0, true, false) else { panic!() };
        assert_eq!((kvs.len(), count), (0, 3));
        let Response::Range { kvs, .. } = r(0, false, true) else { panic!() };
        assert!(kvs.iter().all(|(_, e)| e.value.is_empty()) && kvs.len() == 3);
    }

    #[test]
    fn requests_round_trip_with_any_bytes_and_reject_nested_transactions() {
        let reqs = [
            Request::Put { key: vec![0, 255, 0], value: vec![], prev_kv: false },
            Request::Delete { key: b"a".to_vec(), end: vec![0], prev_kv: true },
            cas("k", u64::MAX, "v"),
            Request::Txn {
                compares: vec![Compare {
                    key: vec![1],
                    cmp: Cmp::Greater,
                    target: Target::Value(vec![0, 1]),
                }],
                success: vec![Request::Delete { key: vec![2], end: Vec::new(), prev_kv: false }],
                failure: vec![get("q")],
            },
        ];
        for r in reqs {
            assert_eq!(Request::decode(&r.encode()), Some(r.clone()));
            let mut longer = r.encode();
            longer.push(0);
            assert_eq!(Request::decode(&longer), None);
        }
        let nested =
            Request::Txn { compares: Vec::new(), success: vec![cas("k", 1, "v")], failure: Vec::new() };
        assert_eq!(Request::decode(&nested.encode()), None);
        assert_eq!(Request::decode(&[9]), None);
    }

    #[test]
    fn replicas_applying_the_same_requests_agree() {
        let reqs = [
            put("a", "1"),
            cas("a", 1, "2"),
            Request::Delete { key: b"a".to_vec(), end: Vec::new(), prev_kv: true },
            put("b", "3"),
        ];
        let (mut x, mut y) = (Kv::default(), Kv::default());
        let rx: Vec<_> = reqs.iter().map(|r| apply(&mut x, r)).collect();
        let ry: Vec<_> = reqs.iter().map(|r| apply(&mut y, &Request::decode(&r.encode()).unwrap())).collect();
        assert_eq!((rx, &x), (ry, &y));
    }
}

//! etcd's v3 KV API against a real 3-process cluster, through the client code generated
//! from the same protos etcd's Go client uses.

mod common;

#[path = "../src/pb.rs"]
mod pb;

use std::time::{Duration, Instant};

use common::Cluster;
use pb::etcdserverpb::kv_client::KvClient;
use pb::etcdserverpb::{
    CompactionRequest, Compare, DeleteRangeRequest, PutRequest, RangeRequest, RequestOp, TxnRequest, compare,
    range_request, request_op, response_op,
};
use tonic::Code;
use tonic::transport::Channel;

async fn connect(c: &Cluster, i: usize) -> KvClient<Channel> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(k) = KvClient::connect(format!("http://127.0.0.1:{}", c.etcd[i])).await {
            return k;
        }
        assert!(Instant::now() < deadline, "replica {i} serves no etcd API");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn put(key: &str, value: &str, prev_kv: bool) -> PutRequest {
    PutRequest { key: key.into(), value: value.into(), prev_kv, ..Default::default() }
}

fn get(key: &str) -> RangeRequest {
    RangeRequest { key: key.into(), ..Default::default() }
}

fn cas(key: &str, mod_revision: i64, value: &str) -> TxnRequest {
    TxnRequest {
        compare: vec![Compare {
            result: compare::CompareResult::Equal as i32,
            target: compare::CompareTarget::Mod as i32,
            key: key.into(),
            target_union: Some(compare::TargetUnion::ModRevision(mod_revision)),
            ..Default::default()
        }],
        success: vec![RequestOp { request: Some(request_op::Request::RequestPut(put(key, value, true))) }],
        failure: vec![RequestOp { request: Some(request_op::Request::RequestRange(get(key))) }],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn etcd_clients_read_write_and_compare_and_swap() {
    let c = Cluster::with_etcd(3);
    let (mut a, mut b) = (connect(&c, 0).await, connect(&c, 1).await);

    // A put on one replica is read linearizably on another, with matching revisions.
    let r = a.put(put("k", "v1", false)).await.unwrap().into_inner();
    let rev = r.header.unwrap().revision;
    let got = b.range(get("k")).await.unwrap().into_inner();
    assert_eq!(got.kvs.len(), 1);
    let kv = &got.kvs[0];
    assert_eq!((&kv.value[..], kv.mod_revision, kv.create_revision, kv.version), (&b"v1"[..], rev, rev, 1));

    // prev_kv on put and delete.
    let r = b.put(put("k", "v2", true)).await.unwrap().into_inner();
    let prev = r.prev_kv.unwrap();
    assert_eq!((&prev.key[..], &prev.value[..], prev.mod_revision), (&b"k"[..], &b"v1"[..], rev));
    let d = a
        .delete_range(DeleteRangeRequest { key: "k".into(), prev_kv: true, ..Default::default() })
        .await
        .unwrap()
        .into_inner();
    assert_eq!((d.deleted, &d.prev_kvs[0].value[..]), (1, &b"v2"[..]));

    // Compare-and-swap on mod revision succeeds once; the stale retry fails and reads.
    let r = a.put(put("lock", "free", false)).await.unwrap().into_inner();
    let at = r.header.unwrap().revision;
    let first = b.txn(cas("lock", at, "mine")).await.unwrap().into_inner();
    assert!(first.succeeded);
    let Some(response_op::Response::ResponsePut(p)) = &first.responses[0].response else {
        panic!("{first:?}")
    };
    assert_eq!(&p.prev_kv.as_ref().unwrap().key[..], b"lock");
    let retry = a.txn(cas("lock", at, "theirs")).await.unwrap().into_inner();
    assert!(!retry.succeeded);
    let Some(response_op::Response::ResponseRange(r)) = &retry.responses[0].response else {
        panic!("{retry:?}")
    };
    assert_eq!(&r.kvs[0].value[..], b"mine");

    // Prefix and all-keys ranges come in key order.
    for k in ["p/2", "p/1", "p/3", "q"] {
        a.put(put(k, "x", false)).await.unwrap();
    }
    let prefix =
        b.range(RangeRequest { key: "p/".into(), range_end: "p0".into(), ..Default::default() }).await;
    let keys: Vec<Vec<u8>> = prefix.unwrap().into_inner().kvs.into_iter().map(|kv| kv.key).collect();
    assert_eq!(keys, [b"p/1".to_vec(), b"p/2".to_vec(), b"p/3".to_vec()]);
    let all = b
        .range(RangeRequest { key: vec![0], range_end: vec![0], count_only: true, ..Default::default() })
        .await;
    assert_eq!(all.unwrap().into_inner().count, 5, "lock, p/1, p/2, p/3, q");

    // A serializable read on the replica that wrote sees its own write.
    a.put(put("s", "local", false)).await.unwrap();
    let s = a.range(RangeRequest { key: "s".into(), serializable: true, ..Default::default() }).await;
    assert_eq!(&s.unwrap().into_inner().kvs[0].value[..], b"local");
}

fn code<T: std::fmt::Debug>(r: Result<tonic::Response<T>, tonic::Status>) -> Code {
    r.unwrap_err().code()
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_outside_the_subset_are_refused_explicitly() {
    let c = Cluster::with_etcd(3);
    let mut a = connect(&c, 0).await;

    assert_eq!(
        code(a.range(RangeRequest { key: "k".into(), revision: 5, ..Default::default() }).await),
        Code::Unimplemented
    );
    let sorted = RangeRequest {
        key: "k".into(),
        sort_order: range_request::SortOrder::Descend as i32,
        ..Default::default()
    };
    assert_eq!(code(a.range(sorted).await), Code::Unimplemented);
    assert_eq!(
        code(a.put(PutRequest { key: "k".into(), lease: 7, ..Default::default() }).await),
        Code::Unimplemented
    );
    let nested = TxnRequest {
        success: vec![RequestOp { request: Some(request_op::Request::RequestTxn(TxnRequest::default())) }],
        ..Default::default()
    };
    assert_eq!(code(a.txn(nested).await), Code::Unimplemented);
    assert_eq!(
        code(a.compact(CompactionRequest { revision: 1, ..Default::default() }).await),
        Code::Unimplemented
    );
    assert_eq!(code(a.put(put("", "v", false)).await), Code::InvalidArgument);
    assert_eq!(code(a.range(get("")).await), Code::InvalidArgument);
}

//! etcd v3's KV service over gRPC: `Range`, `Put`, `DeleteRange` and single-level `Txn`,
//! wire-compatible with etcd's clients. Requests become the same commands the line protocol
//! sends; anything outside that subset is refused with an explicit status, never answered
//! approximately.

use std::sync::mpsc::SyncSender;

use tokio::sync::oneshot;
use tonic::{Request as GrpcRequest, Response as GrpcResponse, Status};

use crate::client::{Cmd, Failure, Reply};
use crate::kv::{Entry, in_range};
use crate::node::Ev;
use crate::ops::{self, Applied, Cmp, Compare, RangeReq, Request, Response, Target};
use crate::pb::etcdserverpb::kv_server::Kv;
use crate::pb::etcdserverpb::{
    CompactionRequest, CompactionResponse, DeleteRangeRequest, DeleteRangeResponse, PutRequest, PutResponse,
    RangeRequest, RangeResponse, RequestOp, ResponseHeader, ResponseOp, TxnRequest, TxnResponse, compare,
    range_request, request_op, response_op,
};
use crate::pb::mvccpb::KeyValue;

pub use crate::pb::etcdserverpb::kv_server::KvServer;

pub struct EtcdKv {
    events: SyncSender<Ev>,
    cluster_id: u64,
    member_id: u64,
}

impl EtcdKv {
    pub fn new(events: SyncSender<Ev>, cluster_id: [u8; 16], member_id: u32) -> Self {
        let mut id = [0u8; 8];
        id.copy_from_slice(&cluster_id[..8]);
        Self { events, cluster_id: u64::from_le_bytes(id), member_id: u64::from(member_id) }
    }

    async fn call(&self, cmd: Cmd) -> Result<Reply, Status> {
        let (reply, rx) = oneshot::channel();
        if !crate::push(&self.events, Ev::Client { cmd, reply }).await {
            return Err(Status::unavailable("replica stopped"));
        }
        rx.await.map_err(|_| Status::unavailable("replica stopped"))
    }

    fn header(&self, revision: u64) -> Option<ResponseHeader> {
        Some(ResponseHeader {
            cluster_id: self.cluster_id,
            member_id: self.member_id,
            revision: revision as i64,
            raft_term: 0,
        })
    }

    /// The applied result of a request, or the status its failure maps to.
    async fn applied(&self, cmd: Cmd) -> Result<Result<Applied, u64>, Status> {
        match self.call(cmd).await? {
            Reply::Done(a) => Ok(Ok(a)),
            Reply::Unknown { revision } => Ok(Err(revision)),
            Reply::Failed(f) => Err(failure(f)),
            Reply::Text(t) => Err(Status::internal(t)),
        }
    }
}

/// Codes etcd's client retries on another endpoint for are used where retrying is safe.
fn failure(f: Failure) -> Status {
    match f {
        Failure::Refused(quepaxa::Error::PayloadTooLarge) => Status::invalid_argument("request is too large"),
        Failure::Refused(e) => Status::unavailable(e.to_string()),
        Failure::OutcomeUnknown => {
            Status::unknown("replica fenced; the request may or may not have committed")
        }
        Failure::Fenced => Status::unavailable("replica fenced"),
    }
}

fn unknown_result() -> Status {
    Status::unknown("committed, but an installed snapshot hid its result")
}

fn key_value(key: &[u8], e: &Entry) -> KeyValue {
    KeyValue {
        key: key.to_vec(),
        create_revision: e.create as i64,
        mod_revision: e.modified as i64,
        version: e.version as i64,
        value: e.value.to_vec(),
        lease: 0,
    }
}

fn kvs(list: &ops::Kvs) -> Vec<KeyValue> {
    list.iter().map(|(k, e)| key_value(k, e)).collect()
}

fn non_empty(key: &[u8]) -> Result<(), Status> {
    if key.is_empty() { Err(Status::invalid_argument("key is not provided")) } else { Ok(()) }
}

fn range_req(r: &RangeRequest) -> Result<RangeReq, Status> {
    non_empty(&r.key)?;
    if r.revision != 0 {
        return Err(Status::unimplemented("past revisions are not kept"));
    }
    if r.sort_order != range_request::SortOrder::None as i32 {
        return Err(Status::unimplemented("sorting is not supported; keys come in key order"));
    }
    if r.min_mod_revision | r.max_mod_revision | r.min_create_revision | r.max_create_revision != 0 {
        return Err(Status::unimplemented("revision filters are not supported"));
    }
    let limit = u64::try_from(r.limit).map_err(|_| Status::invalid_argument("negative limit"))?;
    Ok(RangeReq {
        key: r.key.clone(),
        end: r.range_end.clone(),
        limit,
        count_only: r.count_only,
        keys_only: r.keys_only,
    })
}

fn put_req(r: &PutRequest) -> Result<Request, Status> {
    non_empty(&r.key)?;
    if r.lease != 0 || r.ignore_value || r.ignore_lease {
        return Err(Status::unimplemented("leases are not supported"));
    }
    Ok(Request::Put { key: r.key.clone(), value: r.value.clone(), prev_kv: r.prev_kv })
}

fn delete_req(r: &DeleteRangeRequest) -> Result<Request, Status> {
    non_empty(&r.key)?;
    Ok(Request::Delete { key: r.key.clone(), end: r.range_end.clone(), prev_kv: r.prev_kv })
}

fn compare_req(c: &crate::pb::etcdserverpb::Compare) -> Result<Compare, Status> {
    if !c.range_end.is_empty() {
        return Err(Status::unimplemented("comparisons over a range of keys are not supported"));
    }
    let cmp = match compare::CompareResult::try_from(c.result) {
        Ok(compare::CompareResult::Equal) => Cmp::Equal,
        Ok(compare::CompareResult::NotEqual) => Cmp::NotEqual,
        Ok(compare::CompareResult::Less) => Cmp::Less,
        Ok(compare::CompareResult::Greater) => Cmp::Greater,
        Err(_) => return Err(Status::invalid_argument("unknown comparison")),
    };
    let n = |v: i64| u64::try_from(v).map_err(|_| Status::invalid_argument("negative comparison value"));
    use compare::TargetUnion as U;
    let target = match (compare::CompareTarget::try_from(c.target), &c.target_union) {
        (Ok(compare::CompareTarget::Version), Some(U::Version(v))) => Target::Version(n(*v)?),
        (Ok(compare::CompareTarget::Create), Some(U::CreateRevision(v))) => Target::Create(n(*v)?),
        (Ok(compare::CompareTarget::Mod), Some(U::ModRevision(v))) => Target::Mod(n(*v)?),
        (Ok(compare::CompareTarget::Value), Some(U::Value(v))) => Target::Value(v.clone()),
        (Ok(compare::CompareTarget::Version), None) => Target::Version(0),
        (Ok(compare::CompareTarget::Create), None) => Target::Create(0),
        (Ok(compare::CompareTarget::Mod), None) => Target::Mod(0),
        (Ok(compare::CompareTarget::Value), None) => Target::Value(Vec::new()),
        (Ok(compare::CompareTarget::Lease), _) => {
            return Err(Status::unimplemented("leases are not supported"));
        }
        _ => return Err(Status::invalid_argument("comparison target and value do not match")),
    };
    non_empty(&c.key)?;
    Ok(Compare { key: c.key.clone(), cmp, target })
}

fn op_req(op: &RequestOp) -> Result<Request, Status> {
    match &op.request {
        Some(request_op::Request::RequestRange(r)) => Ok(Request::Range(range_req(r)?)),
        Some(request_op::Request::RequestPut(r)) => put_req(r),
        Some(request_op::Request::RequestDeleteRange(r)) => delete_req(r),
        Some(request_op::Request::RequestTxn(_)) => {
            Err(Status::unimplemented("nested transactions are not supported"))
        }
        None => Err(Status::invalid_argument("empty request operation")),
    }
}

/// etcd refuses a branch that writes a key twice: a key put twice, or put and deleted.
fn distinct_writes(ops: &[Request]) -> Result<(), Status> {
    let puts: Vec<&[u8]> = ops
        .iter()
        .filter_map(|r| if let Request::Put { key, .. } = r { Some(&key[..]) } else { None })
        .collect();
    for (i, key) in puts.iter().enumerate() {
        let deleted =
            ops.iter().any(|r| matches!(r, Request::Delete { key: k, end, .. } if in_range(k, end, key)));
        if deleted || puts[..i].contains(key) {
            return Err(Status::invalid_argument("duplicate key given in txn request"));
        }
    }
    Ok(())
}

/// The response to `op` (one of the executed branch's requests).
fn op_response(r: Response, op: &RequestOp, header: Option<ResponseHeader>) -> ResponseOp {
    let response = match r {
        Response::Range { kvs: list, more, count } => response_op::Response::ResponseRange(RangeResponse {
            header,
            kvs: kvs(&list),
            more,
            count: count as i64,
        }),
        Response::Put { prev } => {
            let key = match &op.request {
                Some(request_op::Request::RequestPut(p)) => &p.key[..],
                _ => &[],
            };
            response_op::Response::ResponsePut(PutResponse {
                header,
                prev_kv: prev.map(|e| key_value(key, &e)),
            })
        }
        Response::Delete { deleted, prev } => {
            response_op::Response::ResponseDeleteRange(DeleteRangeResponse {
                header,
                deleted: deleted as i64,
                prev_kvs: kvs(&prev),
            })
        }
        // Branches hold no transaction (decode refuses one).
        Response::Txn { .. } => unreachable!("nested transaction response"),
    };
    ResponseOp { response: Some(response) }
}

#[tonic::async_trait]
impl Kv for EtcdKv {
    async fn range(&self, req: GrpcRequest<RangeRequest>) -> Result<GrpcResponse<RangeResponse>, Status> {
        let r = req.into_inner();
        let cmd = Cmd::Read { range: range_req(&r)?, linearizable: !r.serializable };
        let a = self.applied(cmd).await?.map_err(|_| unknown_result())?;
        let Response::Range { kvs: list, more, count } = a.response else {
            return Err(Status::internal("not a range"));
        };
        Ok(GrpcResponse::new(RangeResponse {
            header: self.header(a.revision),
            kvs: kvs(&list),
            more,
            count: count as i64,
        }))
    }

    async fn put(&self, req: GrpcRequest<PutRequest>) -> Result<GrpcResponse<PutResponse>, Status> {
        let r = req.into_inner();
        let cmd = Cmd::Write(put_req(&r)?);
        match self.applied(cmd).await? {
            Ok(a) => {
                let Response::Put { prev } = a.response else { return Err(Status::internal("not a put")) };
                let prev_kv = prev.map(|e| key_value(&r.key, &e));
                Ok(GrpcResponse::new(PutResponse { header: self.header(a.revision), prev_kv }))
            }
            Err(revision) if !r.prev_kv => {
                Ok(GrpcResponse::new(PutResponse { header: self.header(revision), prev_kv: None }))
            }
            Err(_) => Err(unknown_result()),
        }
    }

    async fn delete_range(
        &self,
        req: GrpcRequest<DeleteRangeRequest>,
    ) -> Result<GrpcResponse<DeleteRangeResponse>, Status> {
        let r = req.into_inner();
        let a = self.applied(Cmd::Write(delete_req(&r)?)).await?.map_err(|_| unknown_result())?;
        let Response::Delete { deleted, prev } = a.response else {
            return Err(Status::internal("not a delete"));
        };
        Ok(GrpcResponse::new(DeleteRangeResponse {
            header: self.header(a.revision),
            deleted: deleted as i64,
            prev_kvs: kvs(&prev),
        }))
    }

    async fn txn(&self, req: GrpcRequest<TxnRequest>) -> Result<GrpcResponse<TxnResponse>, Status> {
        let r = req.into_inner();
        let compares = r.compare.iter().map(compare_req).collect::<Result<_, _>>()?;
        let success: Vec<Request> = r.success.iter().map(op_req).collect::<Result<_, _>>()?;
        let failure: Vec<Request> = r.failure.iter().map(op_req).collect::<Result<_, _>>()?;
        distinct_writes(&success)?;
        distinct_writes(&failure)?;
        let a = self
            .applied(Cmd::Write(Request::Txn { compares, success, failure }))
            .await?
            .map_err(|_| unknown_result())?;
        let Response::Txn { succeeded, responses } = a.response else {
            return Err(Status::internal("not a txn"));
        };
        let header = self.header(a.revision);
        let branch = if succeeded { &r.success } else { &r.failure };
        let responses =
            responses.into_iter().zip(branch).map(|(resp, op)| op_response(resp, op, header)).collect();
        Ok(GrpcResponse::new(TxnResponse { header, succeeded, responses }))
    }

    async fn compact(
        &self,
        _: GrpcRequest<CompactionRequest>,
    ) -> Result<GrpcResponse<CompactionResponse>, Status> {
        Err(Status::unimplemented("history is not kept, so there is nothing to compact"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_map_to_codes_etcd_clients_handle() {
        let code = |f| failure(f).code();
        assert_eq!(code(Failure::Refused(quepaxa::Error::Busy)), tonic::Code::Unavailable);
        assert_eq!(code(Failure::Refused(quepaxa::Error::Fenced)), tonic::Code::Unavailable);
        assert_eq!(code(Failure::Fenced), tonic::Code::Unavailable);
        assert_eq!(code(Failure::OutcomeUnknown), tonic::Code::Unknown);
        assert_eq!(code(Failure::Refused(quepaxa::Error::PayloadTooLarge)), tonic::Code::InvalidArgument);
    }

    fn pb_compare(
        result: compare::CompareResult,
        target: compare::CompareTarget,
    ) -> crate::pb::etcdserverpb::Compare {
        crate::pb::etcdserverpb::Compare {
            result: result as i32,
            target: target as i32,
            key: b"k".to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn comparisons_translate_each_result_and_target() {
        use compare::{CompareResult as R, CompareTarget as T, TargetUnion as U};
        for (result, cmp) in [
            (R::Equal, Cmp::Equal),
            (R::NotEqual, Cmp::NotEqual),
            (R::Less, Cmp::Less),
            (R::Greater, Cmp::Greater),
        ] {
            let c = compare_req(&pb_compare(result, T::Version)).unwrap();
            assert_eq!((c.cmp, c.target), (cmp, Target::Version(0)), "a missing value compares as zero");
        }
        let with = |target, union| crate::pb::etcdserverpb::Compare {
            target_union: Some(union),
            ..pb_compare(R::Equal, target)
        };
        assert_eq!(compare_req(&with(T::Create, U::CreateRevision(4))).unwrap().target, Target::Create(4));
        assert_eq!(compare_req(&with(T::Mod, U::ModRevision(5))).unwrap().target, Target::Mod(5));
        assert_eq!(
            compare_req(&with(T::Value, U::Value(b"v".to_vec()))).unwrap().target,
            Target::Value(b"v".to_vec())
        );
        let code = |c| compare_req(&c).unwrap_err().code();
        assert_eq!(
            code(with(T::Mod, U::Version(1))),
            tonic::Code::InvalidArgument,
            "target and value disagree"
        );
        assert_eq!(code(with(T::Mod, U::ModRevision(-1))), tonic::Code::InvalidArgument);
        assert_eq!(code(pb_compare(R::Equal, T::Lease)), tonic::Code::Unimplemented);
        let ranged =
            crate::pb::etcdserverpb::Compare { range_end: b"z".to_vec(), ..pb_compare(R::Equal, T::Version) };
        assert_eq!(code(ranged), tonic::Code::Unimplemented);
        let unkeyed =
            crate::pb::etcdserverpb::Compare { key: Vec::new(), ..pb_compare(R::Equal, T::Version) };
        assert_eq!(code(unkeyed), tonic::Code::InvalidArgument);
    }

    #[test]
    fn a_branch_may_not_write_one_key_twice() {
        let put = |k: &[u8]| Request::Put { key: k.to_vec(), value: Vec::new(), prev_kv: false };
        let delete =
            |k: &[u8], end: &[u8]| Request::Delete { key: k.to_vec(), end: end.to_vec(), prev_kv: false };
        let read = Request::Range(RangeReq::key(b"a"));
        assert!(distinct_writes(&[put(b"a"), put(b"b"), read.clone(), delete(b"c", b"")]).is_ok());
        assert!(distinct_writes(&[delete(b"a", b"c"), delete(b"b", b"")]).is_ok(), "deletes may overlap");
        assert!(distinct_writes(&[put(b"a"), read, put(b"a")]).is_err());
        assert!(distinct_writes(&[delete(b"a", b"c"), put(b"b")]).is_err());
        assert!(distinct_writes(&[put(b"z"), delete(b"a", b"\0")]).is_err());
    }
}

//! etcd v3 gRPC <-> internal KV request/response conversions for `openraft-kv`.

use tonic::Status;

use crate::types::pb::etcdserverpb::{
    Compare, DeleteRangeRequest, DeleteRangeResponse, PutRequest, PutResponse, RangeRequest, RangeResponse,
    RequestOp, ResponseHeader, ResponseOp, TxnRequest, compare, range_request, request_op, response_op,
};
use crate::types::pb::mvccpb::KeyValue;
use crate::types::{
    CmpOp, CmpTarget, KvEntry, KvOpResult, KvRequest, RangeQuery, TxnCompare, TxnOp, in_range,
};

pub fn key_value(key: Vec<u8>, e: KvEntry) -> KeyValue {
    KeyValue {
        key,
        create_revision: e.create_revision as i64,
        mod_revision: e.mod_revision as i64,
        version: e.version as i64,
        value: e.value,
        lease: 0,
    }
}

pub fn key_values(list: Vec<(Vec<u8>, KvEntry)>) -> Vec<KeyValue> {
    list.into_iter().map(|(k, e)| key_value(k, e)).collect()
}

pub fn parse_range(r: &RangeRequest) -> Result<RangeQuery, Status> {
    if r.key.is_empty() {
        return Err(Status::invalid_argument("key is not provided"));
    }
    if r.revision != 0 {
        return Err(Status::unimplemented("past revisions are not kept"));
    }
    if r.sort_order != range_request::SortOrder::None as i32 {
        return Err(Status::unimplemented("sorting is not supported"));
    }
    if r.min_mod_revision | r.max_mod_revision | r.min_create_revision | r.max_create_revision != 0 {
        return Err(Status::unimplemented("revision filters are not supported"));
    }
    let limit = u64::try_from(r.limit).map_err(|_| Status::invalid_argument("negative limit"))?;
    Ok(RangeQuery {
        key: r.key.clone(),
        range_end: r.range_end.clone(),
        limit,
        count_only: r.count_only,
        keys_only: r.keys_only,
    })
}

pub fn parse_delete(r: &DeleteRangeRequest) -> Result<KvRequest, Status> {
    if r.key.is_empty() {
        return Err(Status::invalid_argument("key is not provided"));
    }
    Ok(KvRequest::DeleteRange { key: r.key.clone(), range_end: r.range_end.clone(), prev_kv: r.prev_kv })
}

fn parse_compare(c: &Compare) -> Result<TxnCompare, Status> {
    if c.key.is_empty() {
        return Err(Status::invalid_argument("key is not provided"));
    }
    if !c.range_end.is_empty() {
        return Err(Status::unimplemented("comparisons over a range of keys are not supported"));
    }
    let op = match compare::CompareResult::try_from(c.result) {
        Ok(compare::CompareResult::Equal) => CmpOp::Equal,
        Ok(compare::CompareResult::NotEqual) => CmpOp::NotEqual,
        Ok(compare::CompareResult::Less) => CmpOp::Less,
        Ok(compare::CompareResult::Greater) => CmpOp::Greater,
        Err(_) => return Err(Status::invalid_argument("unknown comparison")),
    };
    let non_neg = |v: i64| u64::try_from(v).map_err(|_| Status::invalid_argument("negative value"));
    use compare::TargetUnion as U;
    let target = match (compare::CompareTarget::try_from(c.target), &c.target_union) {
        (Ok(compare::CompareTarget::Version), Some(U::Version(v))) => CmpTarget::Version(non_neg(*v)?),
        (Ok(compare::CompareTarget::Create), Some(U::CreateRevision(v))) => CmpTarget::Create(non_neg(*v)?),
        (Ok(compare::CompareTarget::Mod), Some(U::ModRevision(v))) => CmpTarget::Mod(non_neg(*v)?),
        (Ok(compare::CompareTarget::Value), Some(U::Value(v))) => CmpTarget::Value(v.clone()),
        (Ok(compare::CompareTarget::Version), None) => CmpTarget::Version(0),
        (Ok(compare::CompareTarget::Create), None) => CmpTarget::Create(0),
        (Ok(compare::CompareTarget::Mod), None) => CmpTarget::Mod(0),
        (Ok(compare::CompareTarget::Value), None) => CmpTarget::Value(Vec::new()),
        (Ok(compare::CompareTarget::Lease), _) => {
            return Err(Status::unimplemented("leases are not supported"));
        }
        _ => return Err(Status::invalid_argument("comparison target and value do not match")),
    };
    Ok(TxnCompare { key: c.key.clone(), op, target })
}

fn parse_put_op(r: &PutRequest) -> Result<TxnOp, Status> {
    if r.key.is_empty() {
        return Err(Status::invalid_argument("key is not provided"));
    }
    if r.lease != 0 || r.ignore_value || r.ignore_lease {
        return Err(Status::unimplemented("leases are not supported"));
    }
    Ok(TxnOp::Put { key: r.key.clone(), value: r.value.clone(), prev_kv: r.prev_kv })
}

fn parse_op(op: &RequestOp) -> Result<TxnOp, Status> {
    match &op.request {
        Some(request_op::Request::RequestRange(r)) => Ok(TxnOp::Range(parse_range(r)?)),
        Some(request_op::Request::RequestPut(r)) => parse_put_op(r),
        Some(request_op::Request::RequestDeleteRange(r)) => {
            if r.key.is_empty() {
                return Err(Status::invalid_argument("key is not provided"));
            }
            Ok(TxnOp::Delete { key: r.key.clone(), range_end: r.range_end.clone(), prev_kv: r.prev_kv })
        }
        Some(request_op::Request::RequestTxn(_)) => {
            Err(Status::unimplemented("nested transactions are not supported"))
        }
        None => Err(Status::invalid_argument("empty request operation")),
    }
}

fn check_distinct_writes(ops: &[TxnOp]) -> Result<(), Status> {
    let puts: Vec<&[u8]> = ops
        .iter()
        .filter_map(|o| if let TxnOp::Put { key, .. } = o { Some(&key[..]) } else { None })
        .collect();
    for (i, key) in puts.iter().enumerate() {
        let deleted = ops
            .iter()
            .any(|o| matches!(o, TxnOp::Delete { key: k, range_end, .. } if in_range(k, range_end, key)));
        if deleted || puts[..i].contains(key) {
            return Err(Status::invalid_argument("duplicate key given in txn request"));
        }
    }
    Ok(())
}

pub fn parse_txn(r: &TxnRequest) -> Result<KvRequest, Status> {
    let compares = r.compare.iter().map(parse_compare).collect::<Result<Vec<_>, _>>()?;
    let success = r.success.iter().map(parse_op).collect::<Result<Vec<_>, _>>()?;
    let failure = r.failure.iter().map(parse_op).collect::<Result<Vec<_>, _>>()?;
    check_distinct_writes(&success)?;
    check_distinct_writes(&failure)?;
    Ok(KvRequest::Txn { compares, success, failure })
}

pub fn op_response(res: KvOpResult, header: Option<ResponseHeader>) -> ResponseOp {
    let response = match res {
        KvOpResult::Range { kvs, more, count } => response_op::Response::ResponseRange(RangeResponse {
            header,
            kvs: key_values(kvs),
            more,
            count: count as i64,
        }),
        KvOpResult::Put { prev } => response_op::Response::ResponsePut(PutResponse {
            header,
            prev_kv: prev.map(|(k, e)| key_value(k, e)),
        }),
        KvOpResult::Delete { deleted, prev } => {
            response_op::Response::ResponseDeleteRange(DeleteRangeResponse {
                header,
                deleted: deleted as i64,
                prev_kvs: key_values(prev),
            })
        }
    };
    ResponseOp { response: Some(response) }
}

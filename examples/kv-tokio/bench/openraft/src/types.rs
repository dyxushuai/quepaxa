//! Shared Raft type configuration, KV request/response types, and generated protobuf modules.

use std::fmt;

use serde::{Deserialize, Serialize};

pub type NodeId = u64;

/// Stored value plus etcd-compatible per-key revision metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KvEntry {
    pub value: Vec<u8>,
    pub create_revision: u64,
    pub mod_revision: u64,
    pub version: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RangeQuery {
    pub key: Vec<u8>,
    pub range_end: Vec<u8>,
    pub limit: u64,
    pub count_only: bool,
    pub keys_only: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CmpOp {
    Equal,
    NotEqual,
    Less,
    Greater,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CmpTarget {
    Value(Vec<u8>),
    Version(u64),
    Create(u64),
    Mod(u64),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TxnCompare {
    pub key: Vec<u8>,
    pub op: CmpOp,
    pub target: CmpTarget,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TxnOp {
    Range(RangeQuery),
    Put { key: Vec<u8>, value: Vec<u8>, prev_kv: bool },
    Delete { key: Vec<u8>, range_end: Vec<u8>, prev_kv: bool },
}

impl TxnOp {
    pub fn approx_bytes(&self) -> usize {
        match self {
            Self::Range(q) => 24 + q.key.len() + q.range_end.len(),
            Self::Put { key, value, .. } => 16 + key.len() + value.len(),
            Self::Delete { key, range_end, .. } => 16 + key.len() + range_end.len(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum KvRequest {
    Put { key: Vec<u8>, value: Vec<u8>, prev_kv: bool },
    DeleteRange { key: Vec<u8>, range_end: Vec<u8>, prev_kv: bool },
    Txn { compares: Vec<TxnCompare>, success: Vec<TxnOp>, failure: Vec<TxnOp> },
}

impl KvRequest {
    /// Approximate serialized size in bytes for byte-capped replication batching.
    pub fn approx_bytes(&self) -> usize {
        match self {
            Self::Put { key, value, .. } => 32 + key.len() + value.len(),
            Self::DeleteRange { key, range_end, .. } => 32 + key.len() + range_end.len(),
            Self::Txn { compares, success, failure } => {
                let cmp_bytes: usize = compares
                    .iter()
                    .map(|c| {
                        16 + c.key.len()
                            + match &c.target {
                                CmpTarget::Value(v) => v.len(),
                                _ => 8,
                            }
                    })
                    .sum();
                let ops_bytes: usize = success.iter().chain(failure.iter()).map(TxnOp::approx_bytes).sum();
                48 + cmp_bytes + ops_bytes
            }
        }
    }
}

impl fmt::Display for KvRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Put { key, value, .. } => write!(f, "Put({}B, {}B)", key.len(), value.len()),
            Self::DeleteRange { key, range_end, .. } => {
                write!(f, "DeleteRange({}B..{}B)", key.len(), range_end.len())
            }
            Self::Txn { compares, success, failure } => {
                write!(f, "Txn(cmp={}, ok={}, fail={})", compares.len(), success.len(), failure.len())
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum KvOpResult {
    Range { kvs: Vec<(Vec<u8>, KvEntry)>, more: bool, count: u64 },
    Put { prev: Option<(Vec<u8>, KvEntry)> },
    Delete { deleted: u64, prev: Vec<(Vec<u8>, KvEntry)> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum KvResponseKind {
    Put { prev: Option<(Vec<u8>, KvEntry)> },
    Delete { deleted: u64, prev: Vec<(Vec<u8>, KvEntry)> },
    Txn { succeeded: bool, responses: Vec<KvOpResult> },
}

impl Default for KvResponseKind {
    fn default() -> Self {
        Self::Put { prev: None }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct KvResponse {
    pub revision: u64,
    pub kind: KvResponseKind,
}

/// etcd range inclusion rule for `[start, end)` (`end` empty = single key; `end == [0]` = unbounded).
pub fn in_range(start: &[u8], end: &[u8], candidate: &[u8]) -> bool {
    if end.is_empty() {
        candidate == start
    } else if end == [0] {
        candidate >= start
    } else {
        candidate >= start && candidate < end
    }
}

openraft::declare_raft_types!(
    pub TypeConfig:
        D = KvRequest,
        R = KvResponse,
        NodeId = NodeId,
        Node = openraft::BasicNode,
);

pub type Raft = openraft::Raft<TypeConfig, crate::state_machine::StateMachine>;
pub type LogId = openraft::type_config::alias::LogIdOf<TypeConfig>;
pub type Vote = openraft::type_config::alias::VoteOf<TypeConfig>;
pub type Entry = openraft::type_config::alias::EntryOf<TypeConfig>;
pub type StoredMembership = openraft::type_config::alias::StoredMembershipOf<TypeConfig>;
pub type SnapshotMeta = openraft::type_config::alias::SnapshotMetaOf<TypeConfig>;
pub type Snapshot = openraft::type_config::alias::SnapshotOf<TypeConfig, Vec<u8>>;
pub type CommittedLeaderId = openraft::type_config::alias::CommittedLeaderIdOf<TypeConfig>;

#[allow(clippy::all, dead_code)]
pub mod pb {
    pub mod mvccpb {
        tonic::include_proto!("mvccpb");
    }
    pub mod etcdserverpb {
        tonic::include_proto!("etcdserverpb");
    }
    pub mod openraftkv {
        tonic::include_proto!("openraftkv");
    }
}

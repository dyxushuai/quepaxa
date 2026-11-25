//! Shared Raft type configuration, KV request/response types, and generated protobuf modules.

use std::fmt;

use serde::{Deserialize, Serialize};

pub type NodeId = u64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum KvRequest {
    Put { key: Vec<u8>, value: Vec<u8> },
}

impl fmt::Display for KvRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Put { key, value } => write!(f, "Put({}B, {}B)", key.len(), value.len()),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct KvResponse {
    pub revision: u64,
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
#[cfg(test)]
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

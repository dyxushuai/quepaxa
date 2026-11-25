//! gRPC servers: internal Raft peer/control service (`PeerService`) and etcd v3 KV front end
//! (`EtcdKvService`) with follower-to-leader forwarding.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use openraft::BasicNode;
use openraft::async_runtime::watch::WatchReceiver;
use openraft::errors::{ClientWriteError, LinearizableReadError, RaftError};
use openraft::raft::{AppendEntriesRequest, ReadPolicy, TransferLeaderRequest, VoteRequest};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tonic::{Request, Response, Status, Streaming};

use crate::network::{SnapshotWire, WireAppendResult, make_peer_channel, make_peer_client};
use crate::state_machine::{KvEntry, StateMachine};
use crate::types::pb::etcdserverpb::kv_server::Kv;
use crate::types::pb::etcdserverpb::{
    CompactionRequest, CompactionResponse, DeleteRangeRequest, DeleteRangeResponse, PutRequest, PutResponse,
    RangeRequest, RangeResponse, ResponseHeader, TxnRequest, TxnResponse,
};
use crate::types::pb::mvccpb::KeyValue;
use crate::types::pb::openraftkv::raft_peer_client::RaftPeerClient;
use crate::types::pb::openraftkv::raft_peer_server::RaftPeer;
use crate::types::pb::openraftkv::{
    BytesMsg, Empty, ForwardPutReq, ForwardPutResp, ForwardRangeReq, ForwardRangeResp, MoveLeaderReq,
    StatusResp,
};
use crate::types::{KvRequest, NodeId, Raft, Snapshot, TypeConfig};

/// Matches etcd's internal 7 s proposal timeout so stuck forwards fail boundedly.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(7);

#[derive(Clone)]
pub struct NodeContext {
    pub id: NodeId,
    pub raft: Raft,
    pub sm: StateMachine,
    pub members: Arc<BTreeMap<NodeId, BasicNode>>,
    pub peers: Arc<BTreeMap<NodeId, RaftPeerClient<Channel>>>,
}

impl NodeContext {
    pub fn new(
        id: NodeId,
        raft: Raft,
        sm: StateMachine,
        members: BTreeMap<NodeId, BasicNode>,
    ) -> Result<Self, tonic::transport::Error> {
        let mut peers = BTreeMap::new();
        for (&peer_id, node) in &members {
            if peer_id != id {
                let ch = make_peer_channel(&node.addr)?;
                peers.insert(peer_id, make_peer_client(ch));
            }
        }
        Ok(Self { id, raft, sm, members: Arc::new(members), peers: Arc::new(peers) })
    }

    async fn local_put(&self, key: Vec<u8>, value: Vec<u8>) -> Result<u64, PutOutcome> {
        match self.raft.client_write(KvRequest::Put { key, value }).await {
            Ok(resp) => Ok(resp.data.revision),
            Err(RaftError::APIError(ClientWriteError::ForwardToLeader(fwd))) => {
                Err(PutOutcome::Redirect(fwd.leader_id))
            }
            Err(e) => Err(PutOutcome::Failed(Status::unavailable(e.to_string()))),
        }
    }

    async fn local_linearizable_get(&self, key: &[u8]) -> Result<(u64, Option<KvEntry>), PutOutcome> {
        match self.raft.ensure_linearizable(ReadPolicy::ReadIndex).await {
            Ok(_) => Ok(self.sm.get(key)),
            Err(RaftError::APIError(LinearizableReadError::ForwardToLeader(fwd))) => {
                Err(PutOutcome::Redirect(fwd.leader_id))
            }
            Err(e) => Err(PutOutcome::Failed(Status::unavailable(e.to_string()))),
        }
    }

    /// Forwards a `Put` to `leader_id`, aborting immediately if leadership changes locally
    /// (matching etcd's cancellation of in-flight forwarded proposals on leader loss).
    async fn forward_put(
        &self,
        leader_id: NodeId,
        key: Vec<u8>,
        value: Vec<u8>,
    ) -> Result<ForwardPutResp, Status> {
        let Some(mut peer) = self.peers.get(&leader_id).cloned() else {
            return Err(Status::unavailable(format!("unknown leader {leader_id}")));
        };
        let mut metrics = self.raft.metrics();
        if metrics.borrow_watched().current_leader != Some(leader_id) {
            return Err(Status::unavailable("leader changed while forwarding"));
        }
        let rpc = tokio::time::timeout(FORWARD_TIMEOUT, peer.forward_put(ForwardPutReq { key, value }));
        tokio::select! {
            res = rpc => match res {
                Ok(Ok(resp)) => Ok(resp.into_inner()),
                Ok(Err(status)) => Err(status),
                Err(_) => Err(Status::deadline_exceeded("forward put timed out")),
            },
            () = wait_leader_lost(&mut metrics, leader_id) => {
                Err(Status::unavailable("leader changed while forwarding"))
            }
        }
    }

    async fn forward_range(&self, leader_id: NodeId, key: Vec<u8>) -> Result<ForwardRangeResp, Status> {
        let Some(mut peer) = self.peers.get(&leader_id).cloned() else {
            return Err(Status::unavailable(format!("unknown leader {leader_id}")));
        };
        let mut metrics = self.raft.metrics();
        if metrics.borrow_watched().current_leader != Some(leader_id) {
            return Err(Status::unavailable("leader changed while forwarding"));
        }
        let rpc = tokio::time::timeout(FORWARD_TIMEOUT, peer.forward_range(ForwardRangeReq { key }));
        tokio::select! {
            res = rpc => match res {
                Ok(Ok(resp)) => Ok(resp.into_inner()),
                Ok(Err(status)) => Err(status),
                Err(_) => Err(Status::deadline_exceeded("forward range timed out")),
            },
            () = wait_leader_lost(&mut metrics, leader_id) => {
                Err(Status::unavailable("leader changed while forwarding"))
            }
        }
    }
}

/// Resolves as soon as the watched leader is no longer `Some(expected)`, checking the
/// current value first so a transition right before entering the wait is never missed.
async fn wait_leader_lost<W, R>(metrics: &mut R, expected: NodeId)
where
    W: openraft::async_runtime::watch::Watch,
    R: WatchReceiver<W, openraft::RaftMetrics<TypeConfig>>,
{
    if metrics.borrow_watched().current_leader != Some(expected) {
        return;
    }
    while metrics.changed().await.is_ok() {
        if metrics.borrow_watched().current_leader != Some(expected) {
            return;
        }
    }
    std::future::pending::<()>().await
}

enum PutOutcome {
    Redirect(Option<NodeId>),
    Failed(Status),
}

pub struct PeerService {
    ctx: NodeContext,
}

impl PeerService {
    pub fn new(ctx: NodeContext) -> Self {
        Self { ctx }
    }
}

type AppendOutStream = Pin<Box<dyn Stream<Item = Result<BytesMsg, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl RaftPeer for PeerService {
    type StreamAppendStream = AppendOutStream;

    async fn stream_append(
        &self,
        request: Request<Streaming<BytesMsg>>,
    ) -> Result<Response<Self::StreamAppendStream>, Status> {
        let inbound = request.into_inner();
        let req_stream = inbound.filter_map(|item| async move {
            let msg = item.ok()?;
            serde_json::from_slice::<AppendEntriesRequest<TypeConfig>>(&msg.data).ok()
        });
        let raft = self.ctx.raft.clone();
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(async move {
            let mut out = std::pin::pin!(raft.stream_append(req_stream));
            while let Some(item) = out.next().await {
                let msg = match item {
                    Ok(append_res) => {
                        let wire: WireAppendResult = append_res.map_err(Into::into);
                        match serde_json::to_vec(&wire) {
                            Ok(data) => Ok(BytesMsg { data }),
                            Err(e) => Err(Status::internal(e.to_string())),
                        }
                    }
                    Err(fatal) => Err(Status::unavailable(fatal.to_string())),
                };
                let is_err = msg.is_err();
                if tx.send(msg).await.is_err() || is_err {
                    break;
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn vote(&self, request: Request<BytesMsg>) -> Result<Response<BytesMsg>, Status> {
        let rpc: VoteRequest<TypeConfig> = serde_json::from_slice(&request.into_inner().data)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let resp = self.ctx.raft.vote(rpc).await.map_err(|e| Status::unavailable(e.to_string()))?;
        let data = serde_json::to_vec(&resp).map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(BytesMsg { data }))
    }

    async fn pre_vote(&self, request: Request<BytesMsg>) -> Result<Response<BytesMsg>, Status> {
        let rpc: VoteRequest<TypeConfig> = serde_json::from_slice(&request.into_inner().data)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let resp = self.ctx.raft.pre_vote(rpc).await.map_err(|e| Status::unavailable(e.to_string()))?;
        let data = serde_json::to_vec(&resp).map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(BytesMsg { data }))
    }

    async fn install_snapshot(&self, request: Request<BytesMsg>) -> Result<Response<BytesMsg>, Status> {
        let wire: SnapshotWire = serde_json::from_slice(&request.into_inner().data)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let snap = Snapshot { meta: wire.meta, snapshot: wire.data };
        let resp = self
            .ctx
            .raft
            .install_full_snapshot(wire.vote, snap)
            .await
            .map_err(|e| Status::unavailable(e.to_string()))?;
        let data = serde_json::to_vec(&resp).map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(BytesMsg { data }))
    }

    async fn transfer_leader(&self, request: Request<BytesMsg>) -> Result<Response<BytesMsg>, Status> {
        let req: TransferLeaderRequest<TypeConfig> = serde_json::from_slice(&request.into_inner().data)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let resp = self
            .ctx
            .raft
            .handle_transfer_leader(req)
            .await
            .map_err(|e| Status::unavailable(e.to_string()))?;
        let data = serde_json::to_vec(&resp).map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(BytesMsg { data }))
    }

    async fn forward_put(&self, request: Request<ForwardPutReq>) -> Result<Response<ForwardPutResp>, Status> {
        let req = request.into_inner();
        match self.ctx.local_put(req.key, req.value).await {
            Ok(revision) => {
                Ok(Response::new(ForwardPutResp { revision, redirected: false, leader_id: self.ctx.id }))
            }
            Err(PutOutcome::Redirect(Some(leader_id))) => {
                Ok(Response::new(ForwardPutResp { revision: 0, redirected: true, leader_id }))
            }
            Err(PutOutcome::Redirect(None)) => Err(Status::unavailable("no leader")),
            Err(PutOutcome::Failed(s)) => Err(s),
        }
    }

    async fn forward_range(
        &self,
        request: Request<ForwardRangeReq>,
    ) -> Result<Response<ForwardRangeResp>, Status> {
        let req = request.into_inner();
        match self.ctx.local_linearizable_get(&req.key).await {
            Ok((revision, entry)) => {
                let (found, value, create_revision, mod_revision, version) = match entry {
                    Some(e) => (true, e.value, e.create_revision, e.mod_revision, e.version),
                    None => (false, Vec::new(), 0, 0, 0),
                };
                Ok(Response::new(ForwardRangeResp {
                    revision,
                    found,
                    value,
                    redirected: false,
                    leader_id: self.ctx.id,
                    create_revision,
                    mod_revision,
                    version,
                }))
            }
            Err(PutOutcome::Redirect(Some(leader_id))) => Ok(Response::new(ForwardRangeResp {
                revision: 0,
                found: false,
                value: Vec::new(),
                redirected: true,
                leader_id,
                create_revision: 0,
                mod_revision: 0,
                version: 0,
            })),
            Err(PutOutcome::Redirect(None)) => Err(Status::unavailable("no leader")),
            Err(PutOutcome::Failed(s)) => Err(s),
        }
    }

    async fn init(&self, _request: Request<Empty>) -> Result<Response<Empty>, Status> {
        let members = (*self.ctx.members).clone();
        match self.ctx.raft.initialize(members).await {
            Ok(()) => Ok(Response::new(Empty {})),
            Err(RaftError::APIError(openraft::errors::InitializeError::NotAllowed(_))) => {
                Ok(Response::new(Empty {}))
            }
            Err(e) => Err(Status::unavailable(e.to_string())),
        }
    }

    async fn status(&self, _request: Request<Empty>) -> Result<Response<StatusResp>, Status> {
        let m = self.ctx.raft.metrics().borrow_watched().clone();
        let initialized = self.ctx.raft.is_initialized().await.unwrap_or(false);
        Ok(Response::new(StatusResp {
            node_id: self.ctx.id,
            has_leader: m.current_leader.is_some(),
            leader_id: m.current_leader.unwrap_or(0),
            term: m.current_term,
            initialized,
        }))
    }

    async fn move_leader(&self, request: Request<MoveLeaderReq>) -> Result<Response<Empty>, Status> {
        let target_id = request.into_inner().target_id;
        self.ctx
            .raft
            .trigger()
            .transfer_leader(target_id)
            .await
            .map_err(|e| Status::unavailable(e.to_string()))?;
        Ok(Response::new(Empty {}))
    }
}

pub struct EtcdKvService {
    ctx: NodeContext,
}

impl EtcdKvService {
    pub fn new(ctx: NodeContext) -> Self {
        Self { ctx }
    }

    fn header(&self, revision: u64) -> Option<ResponseHeader> {
        let term = self.ctx.raft.metrics().borrow_watched().current_term;
        Some(ResponseHeader {
            cluster_id: 1,
            member_id: self.ctx.id,
            revision: revision as i64,
            raft_term: term,
        })
    }
}

#[tonic::async_trait]
impl Kv for EtcdKvService {
    async fn put(&self, request: Request<PutRequest>) -> Result<Response<PutResponse>, Status> {
        let req = request.into_inner();
        if req.key.is_empty() {
            return Err(Status::invalid_argument("key is not provided"));
        }
        let mut target = self.ctx.raft.current_leader().await;
        // Follow at most one clean ForwardToLeader redirect (never retry a lost proposal).
        for _ in 0..2 {
            let Some(leader_id) = target else {
                return Err(Status::unavailable("no leader"));
            };
            if leader_id == self.ctx.id {
                match self.ctx.local_put(req.key.clone(), req.value.clone()).await {
                    Ok(revision) => {
                        return Ok(Response::new(PutResponse {
                            header: self.header(revision),
                            prev_kv: None,
                        }));
                    }
                    Err(PutOutcome::Redirect(next)) if next != Some(self.ctx.id) => {
                        target = next;
                        continue;
                    }
                    Err(PutOutcome::Redirect(_)) => return Err(Status::unavailable("no leader")),
                    Err(PutOutcome::Failed(s)) => return Err(s),
                }
            } else {
                let resp = self.ctx.forward_put(leader_id, req.key.clone(), req.value.clone()).await?;
                if resp.redirected && resp.leader_id != leader_id {
                    target = Some(resp.leader_id);
                    continue;
                }
                if resp.redirected {
                    return Err(Status::unavailable("no leader"));
                }
                return Ok(Response::new(PutResponse { header: self.header(resp.revision), prev_kv: None }));
            }
        }
        Err(Status::unavailable("too many leader redirects"))
    }

    async fn range(&self, request: Request<RangeRequest>) -> Result<Response<RangeResponse>, Status> {
        let req = request.into_inner();
        if req.key.is_empty() {
            return Err(Status::invalid_argument("key is not provided"));
        }
        let (revision, val) = if req.serializable {
            self.ctx.sm.get(&req.key)
        } else {
            let mut target = self.ctx.raft.current_leader().await;
            let mut result = None;
            for _ in 0..2 {
                let Some(leader_id) = target else {
                    return Err(Status::unavailable("no leader"));
                };
                if leader_id == self.ctx.id {
                    match self.ctx.local_linearizable_get(&req.key).await {
                        Ok(r) => {
                            result = Some(r);
                            break;
                        }
                        Err(PutOutcome::Redirect(next)) if next != Some(self.ctx.id) => {
                            target = next;
                            continue;
                        }
                        Err(PutOutcome::Redirect(_)) => return Err(Status::unavailable("no leader")),
                        Err(PutOutcome::Failed(s)) => return Err(s),
                    }
                } else {
                    let resp = self.ctx.forward_range(leader_id, req.key.clone()).await?;
                    if resp.redirected && resp.leader_id != leader_id {
                        target = Some(resp.leader_id);
                        continue;
                    }
                    if resp.redirected {
                        return Err(Status::unavailable("no leader"));
                    }
                    let entry = resp.found.then_some(KvEntry {
                        value: resp.value,
                        create_revision: resp.create_revision,
                        mod_revision: resp.mod_revision,
                        version: resp.version,
                    });
                    result = Some((resp.revision, entry));
                    break;
                }
            }
            result.ok_or_else(|| Status::unavailable("too many leader redirects"))?
        };
        let kvs = val
            .map(|entry| {
                vec![KeyValue {
                    key: req.key,
                    create_revision: entry.create_revision as i64,
                    mod_revision: entry.mod_revision as i64,
                    version: entry.version as i64,
                    value: entry.value,
                    lease: 0,
                }]
            })
            .unwrap_or_default();
        let count = kvs.len() as i64;
        Ok(Response::new(RangeResponse { header: self.header(revision), kvs, more: false, count }))
    }

    async fn delete_range(
        &self,
        _request: Request<DeleteRangeRequest>,
    ) -> Result<Response<DeleteRangeResponse>, Status> {
        Err(Status::unimplemented("delete_range is not used by WAN benchmark"))
    }

    async fn txn(&self, _request: Request<TxnRequest>) -> Result<Response<TxnResponse>, Status> {
        Err(Status::unimplemented("txn is not used by WAN benchmark"))
    }

    async fn compact(
        &self,
        _request: Request<CompactionRequest>,
    ) -> Result<Response<CompactionResponse>, Status> {
        Err(Status::unimplemented("compact is not supported"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openraft::async_runtime::watch::WatchSender;
    use openraft::type_config::TypeConfigExt;

    #[tokio::test]
    async fn wait_leader_lost_returns_immediately_when_already_changed() {
        let mut initial = openraft::RaftMetrics::<TypeConfig>::new_initial(0);
        initial.current_leader = None;
        let (_tx, mut rx) = TypeConfig::watch_channel(initial);
        tokio::time::timeout(Duration::from_millis(50), wait_leader_lost(&mut rx, 1))
            .await
            .expect("must not block when current_leader is already != expected");
    }

    #[tokio::test]
    async fn wait_leader_lost_wakes_on_leader_transition() {
        let mut initial = openraft::RaftMetrics::<TypeConfig>::new_initial(0);
        initial.current_leader = Some(1);
        let (tx, mut rx) = TypeConfig::watch_channel(initial.clone());
        let waiter = tokio::spawn(async move {
            wait_leader_lost(&mut rx, 1).await;
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "must wait while leader still matches");
        initial.current_leader = Some(2);
        let _ = tx.send(initial);
        tokio::time::timeout(Duration::from_millis(100), waiter)
            .await
            .expect("must wake on leader change")
            .expect("waiter task ok");
    }
}

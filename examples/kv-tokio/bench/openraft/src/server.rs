//! gRPC servers: internal Raft peer/control service (`PeerService`) and etcd v3 KV front end
//! (`EtcdKvService`) with follower-to-leader forwarding.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use openraft::BasicNode;
use openraft::async_runtime::watch::WatchReceiver;
use openraft::errors::{ClientWriteError, LinearizableReadError, RaftError};
use openraft::raft::{ReadPolicy, TransferLeaderRequest, VoteRequest};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tonic::{Request, Response, Status, Streaming};

use crate::codec::{
    decode_append_entries_req, decode_kv_request, decode_kv_response, decode_snapshot_wire,
    encode_kv_request, encode_kv_response,
};
use crate::etcd_proto::{key_value, key_values, op_response, parse_delete, parse_range, parse_txn};
use crate::network::{WireAppendResult, make_peer_channel, make_peer_client};
use crate::state_machine::{KvEntry, StateMachine};
use crate::types::pb::etcdserverpb::kv_server::Kv;
use crate::types::pb::etcdserverpb::{
    CompactionRequest, CompactionResponse, DeleteRangeRequest, DeleteRangeResponse, PutRequest, PutResponse,
    RangeRequest, RangeResponse, ResponseHeader, TxnRequest, TxnResponse,
};
use crate::types::pb::openraftkv::raft_peer_client::RaftPeerClient;
use crate::types::pb::openraftkv::raft_peer_server::RaftPeer;
use crate::types::pb::openraftkv::{
    BytesMsg, Empty, ForwardKvResp, ForwardPutReq, ForwardPutResp, ForwardRangeReq, ForwardRangeResp,
    MoveLeaderReq, StatusResp,
};
use crate::types::{KvRequest, KvResponse, KvResponseKind, NodeId, Raft, RangeQuery, Snapshot, TypeConfig};

/// Matches etcd's internal 7 s proposal timeout so stuck forwards fail boundedly.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(7);
/// Parallel HTTP/2 channels per peer for follower-to-leader client forwarding.
const PEER_POOL: usize = 8;

#[derive(Clone)]
pub struct NodeContext {
    pub id: NodeId,
    pub raft: Raft,
    pub sm: StateMachine,
    pub members: Arc<BTreeMap<NodeId, BasicNode>>,
    pub peers: Arc<BTreeMap<NodeId, Vec<RaftPeerClient<Channel>>>>,
    rr: Arc<AtomicUsize>,
    leader_rx: watch::Receiver<Option<NodeId>>,
    term: Arc<AtomicU64>,
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
                let mut pool = Vec::with_capacity(PEER_POOL);
                for _ in 0..PEER_POOL {
                    pool.push(make_peer_client(make_peer_channel(&node.addr)?));
                }
                peers.insert(peer_id, pool);
            }
        }
        let mut metrics = raft.metrics();
        let (init_leader, init_term) = {
            let m = metrics.borrow_watched();
            (m.current_leader, m.current_term)
        };
        let (leader_tx, leader_rx) = watch::channel(init_leader);
        let term = Arc::new(AtomicU64::new(init_term));
        let term_bg = term.clone();
        // Coalesce Openraft's per-entry metrics ticks into a single watcher so
        // in-flight forwarded writes only wake when leadership actually changes.
        tokio::spawn(async move {
            while metrics.changed().await.is_ok() {
                let (leader, cur_term) = {
                    let m = metrics.borrow_watched();
                    (m.current_leader, m.current_term)
                };
                term_bg.store(cur_term, Ordering::Relaxed);
                leader_tx.send_if_modified(|cur| {
                    if *cur != leader {
                        *cur = leader;
                        true
                    } else {
                        false
                    }
                });
            }
        });
        Ok(Self {
            id,
            raft,
            sm,
            members: Arc::new(members),
            peers: Arc::new(peers),
            rr: Arc::new(AtomicUsize::new(0)),
            leader_rx,
            term,
        })
    }

    fn current_leader(&self) -> Option<NodeId> {
        *self.leader_rx.borrow()
    }

    fn peer_client(&self, leader_id: NodeId) -> Result<RaftPeerClient<Channel>, Status> {
        let Some(pool) = self.peers.get(&leader_id) else {
            return Err(Status::unavailable(format!("unknown leader {leader_id}")));
        };
        let idx = self.rr.fetch_add(1, Ordering::Relaxed) % pool.len();
        Ok(pool[idx].clone())
    }

    async fn local_kv(&self, req: KvRequest) -> Result<KvResponse, PutOutcome> {
        match self.raft.client_write(req).await {
            Ok(resp) => Ok(resp.data),
            Err(RaftError::APIError(ClientWriteError::ForwardToLeader(fwd))) => {
                Err(PutOutcome::Redirect(fwd.leader_id))
            }
            Err(e) => Err(PutOutcome::Failed(Status::unavailable(e.to_string()))),
        }
    }

    async fn local_put(&self, key: Vec<u8>, value: Vec<u8>) -> Result<u64, PutOutcome> {
        self.local_kv(KvRequest::Put { key, value, prev_kv: false }).await.map(|r| r.revision)
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

    async fn local_linearizable_range(
        &self,
        q: &RangeQuery,
    ) -> Result<(u64, Vec<(Vec<u8>, KvEntry)>, bool, u64), PutOutcome> {
        match self.raft.ensure_linearizable(ReadPolicy::ReadIndex).await {
            Ok(_) => Ok(self.sm.range(q)),
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
        let mut peer = self.peer_client(leader_id)?;
        let mut leader_rx = self.leader_rx.clone();
        if *leader_rx.borrow_and_update() != Some(leader_id) {
            return Err(Status::unavailable("leader changed while forwarding"));
        }
        let rpc = tokio::time::timeout(FORWARD_TIMEOUT, peer.forward_put(ForwardPutReq { key, value }));
        tokio::select! {
            res = rpc => match res {
                Ok(Ok(resp)) => Ok(resp.into_inner()),
                Ok(Err(status)) => Err(status),
                Err(_) => Err(Status::deadline_exceeded("forward put timed out")),
            },
            () = wait_leader_lost(&mut leader_rx, leader_id) => {
                Err(Status::unavailable("leader changed while forwarding"))
            }
        }
    }

    async fn forward_range(&self, leader_id: NodeId, key: Vec<u8>) -> Result<ForwardRangeResp, Status> {
        let mut peer = self.peer_client(leader_id)?;
        let mut leader_rx = self.leader_rx.clone();
        if *leader_rx.borrow_and_update() != Some(leader_id) {
            return Err(Status::unavailable("leader changed while forwarding"));
        }
        let rpc = tokio::time::timeout(FORWARD_TIMEOUT, peer.forward_range(ForwardRangeReq { key }));
        tokio::select! {
            res = rpc => match res {
                Ok(Ok(resp)) => Ok(resp.into_inner()),
                Ok(Err(status)) => Err(status),
                Err(_) => Err(Status::deadline_exceeded("forward range timed out")),
            },
            () = wait_leader_lost(&mut leader_rx, leader_id) => {
                Err(Status::unavailable("leader changed while forwarding"))
            }
        }
    }

    async fn forward_kv(&self, leader_id: NodeId, req: &KvRequest) -> Result<ForwardKvResp, Status> {
        let mut peer = self.peer_client(leader_id)?;
        let mut leader_rx = self.leader_rx.clone();
        if *leader_rx.borrow_and_update() != Some(leader_id) {
            return Err(Status::unavailable("leader changed while forwarding"));
        }
        let data = encode_kv_request(req).map_err(|e| Status::internal(e.to_string()))?;
        let rpc = tokio::time::timeout(FORWARD_TIMEOUT, peer.forward_kv(BytesMsg { data }));
        tokio::select! {
            res = rpc => match res {
                Ok(Ok(resp)) => Ok(resp.into_inner()),
                Ok(Err(status)) => Err(status),
                Err(_) => Err(Status::deadline_exceeded("forward kv timed out")),
            },
            () = wait_leader_lost(&mut leader_rx, leader_id) => {
                Err(Status::unavailable("leader changed while forwarding"))
            }
        }
    }

    /// Submits a general KV write locally if leader or forwards it to the current leader.
    async fn execute_kv(&self, req: KvRequest) -> Result<KvResponse, Status> {
        let mut target = self.current_leader();
        for _ in 0..2 {
            let Some(leader_id) = target else {
                return Err(Status::unavailable("no leader"));
            };
            if leader_id == self.id {
                match self.local_kv(req.clone()).await {
                    Ok(resp) => return Ok(resp),
                    Err(PutOutcome::Redirect(next)) if next != Some(self.id) => {
                        target = next;
                        continue;
                    }
                    Err(PutOutcome::Redirect(_)) => return Err(Status::unavailable("no leader")),
                    Err(PutOutcome::Failed(s)) => return Err(s),
                }
            } else {
                let resp = self.forward_kv(leader_id, &req).await?;
                if resp.redirected && resp.leader_id != leader_id {
                    target = Some(resp.leader_id);
                    continue;
                }
                if resp.redirected {
                    return Err(Status::unavailable("no leader"));
                }
                return decode_kv_response(&resp.data).map_err(|e| Status::internal(e.to_string()));
            }
        }
        Err(Status::unavailable("too many leader redirects"))
    }
}

/// Resolves as soon as the watched leader is no longer `Some(expected)`, checking the
/// current value first so a transition right before entering the wait is never missed.
async fn wait_leader_lost(rx: &mut watch::Receiver<Option<NodeId>>, expected: NodeId) {
    if *rx.borrow_and_update() != Some(expected) {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow_and_update() != Some(expected) {
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
            decode_append_entries_req(&msg.data).ok()
        });
        let raft = self.ctx.raft.clone();
        let (tx, rx) = mpsc::channel(512);
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
        let wire = decode_snapshot_wire(&request.into_inner().data)
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

    async fn forward_kv(&self, request: Request<BytesMsg>) -> Result<Response<ForwardKvResp>, Status> {
        let req = decode_kv_request(&request.into_inner().data)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        match self.ctx.local_kv(req).await {
            Ok(resp) => {
                let data = encode_kv_response(&resp).map_err(|e| Status::internal(e.to_string()))?;
                Ok(Response::new(ForwardKvResp { data, redirected: false, leader_id: self.ctx.id }))
            }
            Err(PutOutcome::Redirect(Some(leader_id))) => {
                Ok(Response::new(ForwardKvResp { data: Vec::new(), redirected: true, leader_id }))
            }
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
        let initialized = m.membership_config.membership().voter_ids().next().is_some();
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
        let term = self.ctx.term.load(Ordering::Relaxed);
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
        if req.prev_kv {
            let resp =
                self.ctx.execute_kv(KvRequest::Put { key: req.key, value: req.value, prev_kv: true }).await?;
            let KvResponseKind::Put { prev } = resp.kind else {
                return Err(Status::internal("unexpected response kind"));
            };
            return Ok(Response::new(PutResponse {
                header: self.header(resp.revision),
                prev_kv: prev.map(|(k, e)| key_value(k, e)),
            }));
        }
        let mut target = self.ctx.current_leader();
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
        let query = parse_range(&req)?;
        if req.serializable {
            let (revision, list, more, count) = self.ctx.sm.range(&query);
            return Ok(Response::new(RangeResponse {
                header: self.header(revision),
                kvs: key_values(list),
                more,
                count: count as i64,
            }));
        }
        if !query.range_end.is_empty() || query.count_only || query.keys_only || query.limit != 0 {
            let leader_id = self.ctx.current_leader().ok_or_else(|| Status::unavailable("no leader"))?;
            if leader_id == self.ctx.id {
                let (revision, list, more, count) =
                    self.ctx.local_linearizable_range(&query).await.map_err(|o| match o {
                        PutOutcome::Redirect(_) => Status::unavailable("leader changed"),
                        PutOutcome::Failed(s) => s,
                    })?;
                return Ok(Response::new(RangeResponse {
                    header: self.header(revision),
                    kvs: key_values(list),
                    more,
                    count: count as i64,
                }));
            }
            let resp = self
                .ctx
                .execute_kv(KvRequest::Txn {
                    compares: Vec::new(),
                    success: vec![crate::types::TxnOp::Range(query)],
                    failure: Vec::new(),
                })
                .await?;
            let KvResponseKind::Txn { mut responses, .. } = resp.kind else {
                return Err(Status::internal("unexpected response kind"));
            };
            let Some(crate::types::KvOpResult::Range { kvs, more, count }) = responses.pop() else {
                return Err(Status::internal("missing range result"));
            };
            return Ok(Response::new(RangeResponse {
                header: self.header(resp.revision),
                kvs: key_values(kvs),
                more,
                count: count as i64,
            }));
        }
        let mut target = self.ctx.current_leader();
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
        let (revision, val) = result.ok_or_else(|| Status::unavailable("too many leader redirects"))?;
        let kvs = val.map(|entry| vec![key_value(req.key, entry)]).unwrap_or_default();
        let count = kvs.len() as i64;
        Ok(Response::new(RangeResponse { header: self.header(revision), kvs, more: false, count }))
    }

    async fn delete_range(
        &self,
        request: Request<DeleteRangeRequest>,
    ) -> Result<Response<DeleteRangeResponse>, Status> {
        let req = parse_delete(&request.into_inner())?;
        let resp = self.ctx.execute_kv(req).await?;
        let KvResponseKind::Delete { deleted, prev } = resp.kind else {
            return Err(Status::internal("unexpected response kind"));
        };
        Ok(Response::new(DeleteRangeResponse {
            header: self.header(resp.revision),
            deleted: deleted as i64,
            prev_kvs: key_values(prev),
        }))
    }

    async fn txn(&self, request: Request<TxnRequest>) -> Result<Response<TxnResponse>, Status> {
        let req = parse_txn(&request.into_inner())?;
        let resp = self.ctx.execute_kv(req).await?;
        let KvResponseKind::Txn { succeeded, responses } = resp.kind else {
            return Err(Status::internal("unexpected response kind"));
        };
        let header = self.header(resp.revision);
        let responses = responses.into_iter().map(|r| op_response(r, header)).collect();
        Ok(Response::new(TxnResponse { header, succeeded, responses }))
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

    #[tokio::test]
    async fn wait_leader_lost_returns_immediately_when_already_changed() {
        let (_tx, mut rx) = watch::channel(None);
        tokio::time::timeout(Duration::from_millis(50), wait_leader_lost(&mut rx, 1))
            .await
            .expect("must not block when current_leader is already != expected");
    }

    #[tokio::test]
    async fn wait_leader_lost_wakes_on_leader_transition() {
        let (tx, mut rx) = watch::channel(Some(1));
        let waiter = tokio::spawn(async move {
            wait_leader_lost(&mut rx, 1).await;
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "must wait while leader still matches");
        let _ = tx.send(Some(2));
        tokio::time::timeout(Duration::from_millis(100), waiter)
            .await
            .expect("must wake on leader change")
            .expect("waiter task ok");
    }
}

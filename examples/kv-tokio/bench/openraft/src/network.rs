//! Tonic gRPC transport implementing Openraft's [`RaftNetworkFactory`] and [`RaftNetworkV2`]
//! with pipelined bidirectional streaming (`StreamAppend`) and isolated heartbeat channels.

use std::future::Future;
use std::io;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use openraft::BasicNode;
use openraft::base::{BoxFuture, BoxStream};
use openraft::errors::{RPCError, ReplicationClosed, StreamingError, Timeout, Unreachable};
use openraft::network::{RPCOption, RaftNetworkFactory, RaftNetworkV2};
use openraft::raft::{
    AppendEntriesRequest, AppendResult, SnapshotResponse, StreamAppendError, TransferLeaderRequest,
    TransferLeaderResponse, VoteRequest, VoteResponse,
};
use tonic::transport::{Channel, Endpoint};

use crate::codec::{encode_append_entries_req, encode_snapshot_wire};
use crate::types::pb::openraftkv::BytesMsg;
use crate::types::pb::openraftkv::raft_peer_client::RaftPeerClient;
use crate::types::{LogId, NodeId, Snapshot, TypeConfig, Vote};

/// Wire format for each `StreamAppend` response item (`AppendResult` without conflict hints).
pub type WireAppendResult = Result<Option<LogId>, StreamAppendError<TypeConfig>>;

const MAX_MSG_BYTES: usize = 64 * 1024 * 1024;

pub fn make_peer_channel(addr: &str) -> Result<Channel, tonic::transport::Error> {
    let uri = if addr.starts_with("http://") || addr.starts_with("https://") {
        addr.to_owned()
    } else {
        format!("http://{addr}")
    };
    let endpoint = Endpoint::from_shared(uri)?
        .tcp_nodelay(true)
        .initial_connection_window_size(64 * 1024 * 1024)
        .initial_stream_window_size(8 * 1024 * 1024)
        .buffer_size(16_384)
        .connect_timeout(Duration::from_secs(2))
        .http2_keep_alive_interval(Duration::from_secs(1))
        .keep_alive_timeout(Duration::from_secs(2))
        .keep_alive_while_idle(true);
    Ok(endpoint.connect_lazy())
}

pub fn make_peer_client(channel: Channel) -> RaftPeerClient<Channel> {
    RaftPeerClient::new(channel)
        .max_decoding_message_size(MAX_MSG_BYTES)
        .max_encoding_message_size(MAX_MSG_BYTES)
}

#[derive(Clone, Default)]
pub struct NetworkFactory;

impl RaftNetworkFactory<TypeConfig> for NetworkFactory {
    type Network = NetworkClient;

    async fn new_client(&mut self, target: NodeId, node: &BasicNode) -> Self::Network {
        NetworkClient::new(target, node.addr.clone())
    }

    async fn new_heartbeat_client(&mut self, target: NodeId, node: &BasicNode) -> Self::Network {
        // Dedicated HTTP/2 connection so heartbeats never queue behind replication batches.
        NetworkClient::new(target, node.addr.clone())
    }

    async fn new_snapshot_client(&mut self, target: NodeId, node: &BasicNode) -> Self::Network {
        NetworkClient::new(target, node.addr.clone())
    }
}

pub struct NetworkClient {
    target: NodeId,
    addr: String,
    client: Option<RaftPeerClient<Channel>>,
}

impl NetworkClient {
    fn new(target: NodeId, addr: String) -> Self {
        Self { target, addr, client: None }
    }

    fn client(&mut self) -> Result<RaftPeerClient<Channel>, RPCError<TypeConfig>> {
        if let Some(c) = &self.client {
            return Ok(c.clone());
        }
        let ch = make_peer_channel(&self.addr).map_err(|e| RPCError::Unreachable(Unreachable::new(&e)))?;
        let c = make_peer_client(ch);
        self.client = Some(c.clone());
        Ok(c)
    }

    fn timeout_err(&self, rpc: openraft::RPCTypes) -> RPCError<TypeConfig> {
        RPCError::Timeout(Timeout { action: rpc, id: 0, target: self.target, timeout: Duration::ZERO })
    }
}

fn io_err(e: io::Error) -> RPCError<TypeConfig> {
    RPCError::Unreachable(Unreachable::new(&e))
}

fn json_err(e: serde_json::Error) -> RPCError<TypeConfig> {
    io_err(io::Error::new(io::ErrorKind::InvalidData, e))
}

fn status_err(s: tonic::Status) -> RPCError<TypeConfig> {
    RPCError::Unreachable(Unreachable::new(&io::Error::other(s.to_string())))
}

impl RaftNetworkV2<TypeConfig> for NetworkClient {
    type SnapshotData = Vec<u8>;

    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<AppendResult<TypeConfig>, RPCError<TypeConfig>> {
        let input = futures_util::stream::once(async move { rpc });
        let mut out = self.stream_append(Box::pin(input), option).await?;
        match out.next().await {
            Some(res) => res,
            None => Err(RPCError::Unreachable(Unreachable::new(&io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "empty stream_append response",
            )))),
        }
    }

    fn stream_append<'s, S>(
        &'s mut self,
        input: S,
        option: RPCOption,
    ) -> BoxFuture<
        's,
        Result<BoxStream<'s, Result<AppendResult<TypeConfig>, RPCError<TypeConfig>>>, RPCError<TypeConfig>>,
    >
    where
        S: Stream<Item = AppendEntriesRequest<TypeConfig>> + Send + Unpin + 'static,
    {
        Box::pin(async move {
            let mut client = self.client()?;
            let req_stream = input.filter_map(|req| async move {
                encode_append_entries_req(&req).ok().map(|data| BytesMsg { data })
            });
            let setup_ttl = option.hard_ttl().max(Duration::from_millis(500));
            let resp = tokio::time::timeout(setup_ttl, client.stream_append(req_stream))
                .await
                .map_err(|_| self.timeout_err(openraft::RPCTypes::AppendEntries))?
                .map_err(status_err)?;
            let inbound = resp.into_inner().map(|item| match item {
                Ok(msg) => {
                    let wire: WireAppendResult = serde_json::from_slice(&msg.data).map_err(json_err)?;
                    Ok(wire.map_err(Into::into))
                }
                Err(status) => Err(status_err(status)),
            });
            Ok(Box::pin(inbound) as BoxStream<'s, _>)
        })
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<VoteResponse<TypeConfig>, RPCError<TypeConfig>> {
        let mut client = self.client()?;
        let data = serde_json::to_vec(&rpc).map_err(json_err)?;
        let resp = tokio::time::timeout(option.soft_ttl(), client.vote(BytesMsg { data }))
            .await
            .map_err(|_| self.timeout_err(openraft::RPCTypes::Vote))?
            .map_err(status_err)?;
        serde_json::from_slice(&resp.into_inner().data).map_err(json_err)
    }

    async fn pre_vote(
        &mut self,
        rpc: VoteRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<VoteResponse<TypeConfig>, RPCError<TypeConfig>> {
        let mut client = self.client()?;
        let data = serde_json::to_vec(&rpc).map_err(json_err)?;
        let resp = tokio::time::timeout(option.soft_ttl(), client.pre_vote(BytesMsg { data }))
            .await
            .map_err(|_| self.timeout_err(openraft::RPCTypes::Vote))?
            .map_err(status_err)?;
        serde_json::from_slice(&resp.into_inner().data).map_err(json_err)
    }

    async fn full_snapshot(
        &mut self,
        vote: Vote,
        snapshot: Snapshot,
        cancel: impl Future<Output = ReplicationClosed> + Send + 'static,
        option: RPCOption,
    ) -> Result<SnapshotResponse<TypeConfig>, StreamingError<TypeConfig>> {
        let mut client = self.client().map_err(StreamingError::from)?;
        let data = encode_snapshot_wire(&vote, &snapshot.meta, &snapshot.snapshot)
            .map_err(io_err)
            .map_err(StreamingError::from)?;
        let ttl = option.soft_ttl().max(Duration::from_secs(5));
        let rpc_fut = tokio::time::timeout(ttl, client.install_snapshot(BytesMsg { data }));
        tokio::select! {
            closed = cancel => Err(StreamingError::Closed(closed)),
            res = rpc_fut => {
                let resp = res
                    .map_err(|_| StreamingError::from(self.timeout_err(openraft::RPCTypes::InstallSnapshot)))?
                    .map_err(|s| StreamingError::from(status_err(s)))?;
                serde_json::from_slice(&resp.into_inner().data)
                    .map_err(json_err)
                    .map_err(StreamingError::from)
            }
        }
    }

    async fn transfer_leader(
        &mut self,
        req: TransferLeaderRequest<TypeConfig>,
        option: RPCOption,
    ) -> Result<TransferLeaderResponse<TypeConfig>, RPCError<TypeConfig>> {
        let mut client = self.client()?;
        let data = serde_json::to_vec(&req).map_err(json_err)?;
        let resp = tokio::time::timeout(option.soft_ttl(), client.transfer_leader(BytesMsg { data }))
            .await
            .map_err(|_| self.timeout_err(openraft::RPCTypes::TransferLeader))?
            .map_err(status_err)?;
        serde_json::from_slice(&resp.into_inner().data).map_err(json_err)
    }
}

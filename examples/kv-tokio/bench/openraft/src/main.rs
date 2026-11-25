//! `openraft-kv`: minimal fsync-backed Openraft KV server serving etcd's v3 KV gRPC API for
//! apples-to-apples WAN benchmarking against `kv-tokio` (QuePaxa) and `etcd`.
//!
//! Server mode:
//!   openraft-kv --id 0 --members 0=172.31.251.10:7000,... --peer 0.0.0.0:7000
//!               --etcd 0.0.0.0:2379 --data /data
//!               [--heartbeat-ms 400] [--election-min-ms 1200] [--election-max-ms 2000]
//!               [--snapshot-every 10000]
//!
//! Control subcommand:
//!   openraft-kv ctl --endpoint 172.31.251.10:7000 init
//!   openraft-kv ctl --endpoint 172.31.251.10:7000 leader
//!   openraft-kv ctl --endpoint 172.31.251.10:7000 transfer <node_id>

mod network;
mod server;
mod state_machine;
mod store;
mod types;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use openraft::{BasicNode, Config, SnapshotPolicy};
use tonic::transport::Server;

use crate::network::{NetworkFactory, make_peer_channel, make_peer_client};
use crate::server::{EtcdKvService, NodeContext, PeerService};
use crate::state_machine::StateMachine;
use crate::store::LogStore;
use crate::types::pb::etcdserverpb::kv_server::KvServer;
use crate::types::pb::openraftkv::raft_peer_server::RaftPeerServer;
use crate::types::pb::openraftkv::{Empty, MoveLeaderReq};
use crate::types::{NodeId, Raft};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const MAX_MSG_BYTES: usize = 64 * 1024 * 1024;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("ctl") {
        return run_ctl(&args[1..]).await;
    }
    run_server(&args).await
}

async fn run_ctl(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut endpoint = String::new();
    let mut cmd = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--endpoint" {
            endpoint = args.get(i + 1).cloned().ok_or("--endpoint requires a value")?;
            i += 2;
        } else {
            cmd.push(args[i].as_str());
            i += 1;
        }
    }
    if endpoint.is_empty() {
        return Err("ctl requires --endpoint <host:port>".into());
    }
    let ch = make_peer_channel(&endpoint)?;
    let mut client = make_peer_client(ch);
    match cmd.as_slice() {
        ["init"] => {
            tokio::time::timeout(Duration::from_secs(10), client.init(Empty {}))
                .await
                .map_err(|_| "init timed out")??;
        }
        ["leader"] => {
            let resp = tokio::time::timeout(Duration::from_secs(5), client.status(Empty {}))
                .await
                .map_err(|_| "status timed out")??
                .into_inner();
            if resp.has_leader {
                println!("{}", resp.leader_id);
            } else {
                println!("none");
            }
        }
        ["transfer", target] => {
            let target_id: NodeId = target.parse()?;
            tokio::time::timeout(Duration::from_secs(10), client.move_leader(MoveLeaderReq { target_id }))
                .await
                .map_err(|_| "transfer timed out")??;
        }
        _ => return Err(format!("unknown ctl command: {cmd:?}").into()),
    }
    Ok(())
}

async fn run_server(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut id: Option<NodeId> = None;
    let mut members: BTreeMap<NodeId, BasicNode> = BTreeMap::new();
    let mut peer_addr: SocketAddr = "0.0.0.0:7000".parse()?;
    let mut etcd_addr: SocketAddr = "0.0.0.0:2379".parse()?;
    let mut data_dir = PathBuf::from("/data");
    let mut heartbeat_ms: u64 = 400;
    let mut election_min_ms: u64 = 1000;
    let mut election_max_ms: u64 = 1500;
    let mut snapshot_every: u64 = 10_000;

    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let val = args.get(i + 1).ok_or_else(|| format!("missing value for {flag}"))?;
        match flag {
            "--id" => id = Some(val.parse()?),
            "--members" => {
                for item in val.split(',').filter(|s| !s.is_empty()) {
                    let (nid, addr) = item.split_once('=').ok_or("expected id=host:port")?;
                    members.insert(nid.parse()?, BasicNode { addr: addr.to_owned() });
                }
            }
            "--peer" => peer_addr = val.parse()?,
            "--etcd" => etcd_addr = val.parse()?,
            "--data" => data_dir = PathBuf::from(val),
            "--heartbeat-ms" => heartbeat_ms = val.parse()?,
            "--election-min-ms" => election_min_ms = val.parse()?,
            "--election-max-ms" => election_max_ms = val.parse()?,
            "--snapshot-every" => snapshot_every = val.parse()?,
            other => return Err(format!("unknown flag {other}").into()),
        }
        i += 2;
    }

    let id = id.ok_or("--id is required")?;
    if members.is_empty() {
        return Err("--members is required".into());
    }

    let config = Config {
        cluster_name: "quepaxa-openraft-wan".to_owned(),
        heartbeat_interval: heartbeat_ms,
        election_timeout_min: election_min_ms,
        election_timeout_max: election_max_ms,
        install_snapshot_timeout: 5_000,
        snapshot_policy: SnapshotPolicy::LogsSinceLast(snapshot_every),
        enable_pre_vote: Some(true),
        ..Default::default()
    };
    let config = Arc::new(config.validate()?);

    let log_store = LogStore::open(&data_dir)?;
    let sm = StateMachine::open(&data_dir)?;
    let raft = Raft::new(id, config, NetworkFactory, log_store, sm.clone()).await?;
    let ctx = NodeContext::new(id, raft, sm, members)?;

    let peer_srv = Server::builder()
        .tcp_nodelay(true)
        .add_service(
            RaftPeerServer::new(PeerService::new(ctx.clone()))
                .max_decoding_message_size(MAX_MSG_BYTES)
                .max_encoding_message_size(MAX_MSG_BYTES),
        )
        .serve(peer_addr);

    let etcd_srv = Server::builder()
        .tcp_nodelay(true)
        .add_service(KvServer::new(EtcdKvService::new(ctx)))
        .serve(etcd_addr);

    tokio::try_join!(peer_srv, etcd_srv)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::pb::etcdserverpb::kv_client::KvClient;
    use crate::types::pb::etcdserverpb::{PutRequest, RangeRequest};
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::TcpListenerStream;

    async fn spawn_node(
        id: NodeId,
        members: BTreeMap<NodeId, BasicNode>,
        peer_listener: TcpListener,
        etcd_listener: TcpListener,
        dir: PathBuf,
    ) -> tokio::task::JoinHandle<()> {
        let config = Arc::new(
            Config {
                cluster_name: "test-cluster".to_owned(),
                heartbeat_interval: 50,
                election_timeout_min: 150,
                election_timeout_max: 300,
                enable_pre_vote: Some(true),
                ..Default::default()
            }
            .validate()
            .expect("valid config"),
        );
        let log_store = LogStore::open(&dir).expect("open log");
        let sm = StateMachine::open(&dir).expect("open sm");
        let raft = Raft::new(id, config, NetworkFactory, log_store, sm.clone()).await.expect("start raft");
        let ctx = NodeContext::new(id, raft, sm, members).expect("ctx");
        tokio::spawn(async move {
            let peer_srv = Server::builder()
                .tcp_nodelay(true)
                .add_service(RaftPeerServer::new(PeerService::new(ctx.clone())))
                .serve_with_incoming(TcpListenerStream::new(peer_listener));
            let etcd_srv = Server::builder()
                .tcp_nodelay(true)
                .add_service(KvServer::new(EtcdKvService::new(ctx)))
                .serve_with_incoming(TcpListenerStream::new(etcd_listener));
            let _ = tokio::try_join!(peer_srv, etcd_srv);
        })
    }

    #[tokio::test]
    async fn three_node_cluster_writes_forwards_reads_and_transfers_leader() {
        let base = std::env::temp_dir().join(format!("openraft-kv-it-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut peer_listeners = Vec::new();
        let mut etcd_listeners = Vec::new();
        let mut members = BTreeMap::new();
        let mut etcd_addrs = Vec::new();
        for id in 0..3u64 {
            let pl = TcpListener::bind("127.0.0.1:0").await.expect("bind peer");
            let el = TcpListener::bind("127.0.0.1:0").await.expect("bind etcd");
            members.insert(id, BasicNode { addr: pl.local_addr().unwrap().to_string() });
            etcd_addrs.push(el.local_addr().unwrap().to_string());
            peer_listeners.push(pl);
            etcd_listeners.push(el);
        }

        let mut tasks = Vec::new();
        for (idx, (pl, el)) in peer_listeners.into_iter().zip(etcd_listeners).enumerate() {
            let id = idx as u64;
            tasks.push(spawn_node(id, members.clone(), pl, el, base.join(format!("n{id}"))).await);
        }

        // Initialize on node 0.
        let mut ctl0 = make_peer_client(make_peer_channel(&members[&0].addr).expect("ch"));
        ctl0.init(Empty {}).await.expect("init cluster");

        // Wait for a leader to be elected.
        let mut leader = None;
        for _ in 0..40 {
            let st = ctl0.status(Empty {}).await.expect("status").into_inner();
            if st.has_leader {
                leader = Some(st.leader_id);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let leader = leader.expect("leader elected");
        let follower = (leader + 1) % 3;

        // Write via follower (exercises ForwardPut) and read via leader (linearizable).
        let mut follower_kv = KvClient::connect(format!("http://{}", etcd_addrs[follower as usize]))
            .await
            .expect("connect follower kv");
        follower_kv
            .put(PutRequest { key: b"hello".to_vec(), value: b"v1".to_vec(), ..Default::default() })
            .await
            .expect("follower put 1");
        follower_kv
            .put(PutRequest { key: b"other".to_vec(), value: b"x".to_vec(), ..Default::default() })
            .await
            .expect("follower put other");
        follower_kv
            .put(PutRequest { key: b"hello".to_vec(), value: b"world".to_vec(), ..Default::default() })
            .await
            .expect("follower put 2");

        let mut leader_kv = KvClient::connect(format!("http://{}", etcd_addrs[leader as usize]))
            .await
            .expect("connect leader kv");
        for client in [&mut leader_kv, &mut follower_kv] {
            let range = client
                .range(RangeRequest { key: b"hello".to_vec(), ..Default::default() })
                .await
                .expect("linearizable range")
                .into_inner();
            assert_eq!(range.header.expect("header").revision, 3);
            assert_eq!(range.kvs.len(), 1);
            assert_eq!(range.kvs[0].value, b"world");
            assert_eq!(range.kvs[0].create_revision, 1);
            assert_eq!(range.kvs[0].mod_revision, 3);
            assert_eq!(range.kvs[0].version, 2);
        }

        // Transfer leadership to follower and verify leader changes.
        let mut ctl_leader = make_peer_client(make_peer_channel(&members[&leader].addr).expect("ch"));
        ctl_leader.move_leader(MoveLeaderReq { target_id: follower }).await.expect("transfer leader");
        let mut moved = false;
        for _ in 0..40 {
            let st = ctl0.status(Empty {}).await.expect("status").into_inner();
            if st.has_leader && st.leader_id == follower {
                moved = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(moved, "leadership should transfer from {leader} to {follower}");

        for t in tasks {
            t.abort();
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}

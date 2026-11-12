//! A cluster of real `kv-tokio` processes on loopback, for the end-to-end tests.

#![allow(dead_code)] // each test uses a different part

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Ports handed to replicas come from below the OS ephemeral range (Linux 32768–60999,
/// macOS 49152–65535), so no outgoing connection, here or in a parallel test, is given one
/// as its local port while a replica is starting or down.
const PORTS: std::ops::Range<u16> = 20_000..32_768;

/// `n` ports that no other cluster, in this or a parallel test process, will use while the
/// returned locks are held. A port's lock file is held for the cluster's lifetime, so a
/// killed replica can restart on the same port; the OS drops it if the test process dies.
fn reserve_ports(n: usize) -> (Vec<u16>, Vec<File>) {
    let dir = std::env::temp_dir().join("kv-tokio-ports");
    std::fs::create_dir_all(&dir).unwrap();
    let span = PORTS.end - PORTS.start;
    let first = (std::process::id() % u32::from(span)) as u16;
    let (mut ports, mut locks) = (Vec::new(), Vec::new());
    for k in 0..span {
        let port = PORTS.start + (first + k) % span;
        let lock = File::create(dir.join(port.to_string())).unwrap();
        // The bind skips a port some unrelated process is using.
        if lock.try_lock().is_ok() && TcpListener::bind(("127.0.0.1", port)).is_ok() {
            ports.push(port);
            locks.push(lock);
            if ports.len() == n {
                return (ports, locks);
            }
        }
    }
    panic!("fewer than {n} ports free in {PORTS:?}");
}

pub struct Cluster {
    peers: Vec<u16>,
    clients: Vec<u16>,
    /// Where each replica serves etcd's v3 KV gRPC API; empty unless asked for
    /// ([`Cluster::with_etcd`]).
    pub etcd: Vec<u16>,
    /// The first `members` replicas are configured; the others may join later.
    members: usize,
    dir: std::path::PathBuf,
    _ports: Vec<File>,
    nodes: Vec<Option<Child>>,
}

impl Cluster {
    /// `n` replicas, all configured members.
    pub fn new(n: usize) -> Self {
        Self::with_members(n, n)
    }

    /// `n` replicas that also serve etcd's v3 KV gRPC API.
    pub fn with_etcd(n: usize) -> Self {
        Self::build(n, n, true)
    }

    /// `n` replicas of which the first `members` are configured; the others start as
    /// joining replicas.
    pub fn with_members(n: usize, members: usize) -> Self {
        Self::build(n, members, false)
    }

    fn build(n: usize, members: usize, etcd: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("kv-tokio-cluster-{}-{n}-{members}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (mut ports, locks) = reserve_ports(if etcd { 3 * n } else { 2 * n });
        let etcd = ports.split_off(2 * n);
        let clients = ports.split_off(n);
        let peers = ports;
        let mut c =
            Self { peers, clients, etcd, members, dir, _ports: locks, nodes: (0..n).map(|_| None).collect() };
        for i in 0..n {
            c.start(i);
        }
        c
    }

    pub fn start(&mut self, i: usize) {
        let addrs: Vec<String> =
            self.peers.iter().enumerate().map(|(j, p)| format!("{j}=127.0.0.1:{p}")).collect();
        let (members, joiners) = addrs.split_at(self.members);
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kv-tokio"));
        cmd.args(["--id", &i.to_string(), "--members", &members.join(",")]);
        if !joiners.is_empty() {
            cmd.args(["--peers", &joiners.join(",")]);
        }
        cmd.args(["--client", &format!("127.0.0.1:{}", self.clients[i])]);
        if let Some(port) = self.etcd.get(i) {
            cmd.args(["--etcd", &format!("127.0.0.1:{port}")]);
        }
        let child = cmd
            .args(["--data", self.dir.join(format!("n{i}")).to_str().unwrap()])
            .args(["--snapshot-every", "50"])
            .spawn()
            .unwrap();
        self.nodes[i] = Some(child);
    }

    pub fn kill(&mut self, i: usize) {
        if let Some(mut c) = self.nodes[i].take() {
            c.kill().unwrap(); // SIGKILL
            c.wait().unwrap();
        }
    }

    /// Waits for replica `i` to exit by itself; returns whether it exited successfully.
    pub fn wait_exit(&mut self, i: usize, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        let child = self.nodes[i].as_mut().expect("started");
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                self.nodes[i] = None;
                return status.success();
            }
            assert!(Instant::now() < deadline, "replica {i} did not exit");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Sends one command to replica `i` once and returns its answer, errors included.
    pub fn request_once(&self, i: usize, cmd: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", self.clients[i])).expect("replica is up");
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(format!("{cmd}\n").as_bytes()).unwrap();
        let mut line = String::new();
        BufReader::new(&s).read_line(&mut line).unwrap();
        line.trim().to_owned()
    }

    /// Sends one command to replica `i`, retrying until it answers (startup, failover).
    pub fn request(&self, i: usize, cmd: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            // While the replica is down its port is free, and a connect may be given that
            // same port as its own and reach itself (TCP simultaneous open), echoing `cmd`.
            let connected = TcpStream::connect(("127.0.0.1", self.clients[i]))
                .ok()
                .filter(|s| s.local_addr().ok() != s.peer_addr().ok());
            if let Some(mut s) = connected {
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                if s.write_all(format!("{cmd}\n").as_bytes()).is_ok() {
                    let mut line = String::new();
                    if BufReader::new(&s).read_line(&mut line).is_ok()
                        && !line.is_empty()
                        && !line.starts_with("ERR")
                    {
                        return line.trim().to_owned();
                    }
                }
            }
            assert!(Instant::now() < deadline, "replica {i} did not answer {cmd}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        for i in 0..self.nodes.len() {
            self.kill(i);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

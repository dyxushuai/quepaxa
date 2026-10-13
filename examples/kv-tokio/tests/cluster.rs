//! Runs a real 3-process cluster: writes, linearizable reads, `kill -9` of a minority,
//! and restart of the killed replica from its data directory.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// `n` distinct free ports: all are held at once, so none repeats, then released for the
/// children to bind.
fn free_ports(n: usize) -> Vec<u16> {
    let held: Vec<TcpListener> = (0..n).map(|_| TcpListener::bind("127.0.0.1:0").unwrap()).collect();
    held.iter().map(|l| l.local_addr().unwrap().port()).collect()
}

struct Cluster {
    peers: Vec<u16>,
    clients: Vec<u16>,
    dir: std::path::PathBuf,
    nodes: Vec<Option<Child>>,
}

impl Cluster {
    fn new(n: usize) -> Self {
        let dir = std::env::temp_dir().join(format!("kv-tokio-cluster-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut ports = free_ports(2 * n);
        let clients = ports.split_off(n);
        let peers = ports;
        let mut c = Self { peers, clients, dir, nodes: (0..n).map(|_| None).collect() };
        for i in 0..n {
            c.start(i);
        }
        c
    }

    fn start(&mut self, i: usize) {
        let members: Vec<String> =
            self.peers.iter().enumerate().map(|(j, p)| format!("{j}=127.0.0.1:{p}")).collect();
        let child = Command::new(env!("CARGO_BIN_EXE_kv-tokio"))
            .args(["--id", &i.to_string(), "--members", &members.join(",")])
            .args(["--client", &format!("127.0.0.1:{}", self.clients[i])])
            .args(["--data", self.dir.join(format!("n{i}")).to_str().unwrap()])
            .args(["--snapshot-every", "50"])
            .spawn()
            .unwrap();
        self.nodes[i] = Some(child);
    }

    fn kill(&mut self, i: usize) {
        if let Some(mut c) = self.nodes[i].take() {
            c.kill().unwrap(); // SIGKILL
            c.wait().unwrap();
        }
    }

    /// Sends one command to replica `i`, retrying until it answers (startup, failover).
    fn request(&self, i: usize, cmd: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(mut s) = TcpStream::connect(("127.0.0.1", self.clients[i])) {
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

#[test]
fn survives_kill_9_of_a_minority_and_recovers_the_victim() {
    let mut c = Cluster::new(3);
    for k in 0..60 {
        assert_eq!(c.request(k % 3, &format!("PUT k{k} v{k}")), "OK");
    }
    assert_eq!(c.request(2, "GET k7"), "VALUE v7");

    // The initial leader (node 0) dies hard; the survivors keep committing.
    c.kill(0);
    for k in 60..120 {
        assert_eq!(c.request(1 + k % 2, &format!("PUT k{k} v{k}")), "OK");
    }
    assert_eq!(c.request(1, "GET k100"), "VALUE v100");

    // The victim restarts from disk and catches up on what it missed.
    c.start(0);
    assert_eq!(c.request(0, "GET k119"), "VALUE v119");
    assert_eq!(c.request(0, "GET k3"), "VALUE v3");
}

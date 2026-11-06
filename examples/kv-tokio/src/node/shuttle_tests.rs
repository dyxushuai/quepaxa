//! The writer handoff and snapshot saves under shuttle's schedulers: DFS explores every
//! interleaving of the handoff alone and of snapshot saves; PCT samples a whole node's,
//! whose replica adds too many steps for DFS to finish. A failure prints a schedule that
//! `shuttle::replay` reproduces.

use std::io;

use quepaxa::{Config, NodeId};
use tokio::sync::oneshot;

use super::*;
use crate::client::{Cmd, Reply};
use crate::ops::Request;

/// Runs `f` under every interleaving.
fn dfs(f: impl Fn() + Send + Sync + 'static) {
    runner(shuttle::scheduler::DfsScheduler::new(None, false)).run(f);
}

/// Runs `f` under `iterations` PCT schedules from a fixed seed.
fn pct(iterations: usize, f: impl Fn() + Send + Sync + 'static) {
    runner(shuttle::scheduler::PctScheduler::new_from_seed(7, 3, iterations)).run(f);
}

fn runner<S: shuttle::scheduler::Scheduler + 'static>(scheduler: S) -> shuttle::Runner<S> {
    let mut config = shuttle::Config::new();
    // The replica's calls need more than the default 60 KiB of stack.
    config.stack_size = 1 << 20;
    shuttle::Runner::new(scheduler, config)
}

/// A record log in memory that fails its `fail_at`-th append (1-based).
#[derive(Clone, Default)]
struct MemLog {
    /// The records, and how many appends were attempted. A std lock: the log's own locking
    /// is no part of the handoff, and every shuttle lock would multiply the schedules.
    state: Arc<std::sync::Mutex<(Vec<Bytes>, usize)>>,
    fail_at: Option<usize>,
}

impl Log for MemLog {
    fn append(&mut self, records: &[Bytes]) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.1 += 1;
        if self.fail_at == Some(state.1) {
            return Err(io::Error::other("the disk failed"));
        }
        state.0.extend_from_slice(records);
        Ok(())
    }

    fn replace(&mut self, records: &[Bytes]) -> io::Result<()> {
        self.state.lock().unwrap().0 = records.to_vec();
        Ok(())
    }
}

/// What a run of `writes` produced.
struct Outcome {
    replies: Vec<Option<Reply>>,
    /// Appends the writer attempted.
    appends: usize,
    fenced: bool,
}

/// A single-member node writing through `log`, as its event loop would: `writes` client
/// writes, each flushed at once, then events taken until every write is answered or the
/// storage failure has fenced the replica. The event queue holds one event, and one other
/// event is queued at the start, so the writer may find it full when it reports.
fn writes(count: usize, max_unwritten: u64, log: MemLog) -> Outcome {
    let cfg = Config::new([1; 16], NodeId(0), vec![NodeId(0)], [2; 32]);
    let replica = Replica::new(cfg, [], Timestamp(0)).unwrap();
    let (events, rx) = mpsc::sync_channel(1);
    events.send(Ev::Written).unwrap();
    let snapshots = SnapshotWriter::new(&std::env::temp_dir());
    let mut n = Node::with(replica, None, snapshots, HashMap::new(), u64::MAX, events);
    n.max_unwritten = max_unwritten;
    n.drain();
    let state = log.state.clone();
    n.start_writer(log);
    let mut answers = Vec::new();
    for i in 0..count {
        let (reply, answer) = oneshot::channel();
        let put = Request::Put { key: format!("k{i}").into_bytes(), value: b"v".to_vec(), prev_kv: false };
        n.on_client(n.now(), Cmd::Write(put), reply);
        n.drain();
        n.flush();
        answers.push(answer);
    }
    let mut replies: Vec<Option<Reply>> = (0..count).map(|_| None).collect();
    loop {
        for (r, a) in replies.iter_mut().zip(&mut answers) {
            if r.is_none() {
                *r = a.try_recv().ok();
            }
        }
        let fenced = n.jobs.is_none();
        if fenced || replies.iter().all(Option::is_some) {
            let appends = state.lock().unwrap().1;
            return Outcome { replies, appends, fenced };
        }
        // Blocks until the writer reports or the queued event is taken; a lost wake-up
        // leaves this thread blocked for good, which shuttle reports as a deadlock.
        let _ = rx.recv();
        n.on_written();
        n.flush();
    }
}

/// The handoff alone: the event loop flushes `persists` persists of 10 bytes each, waiting
/// as `flush` does whenever more than `max_unwritten` bytes are unwritten, and takes reports
/// until the last (or a failure) is reported. Returns the reports in the order taken and
/// the appends attempted. No replica runs, so DFS can explore every interleaving.
fn handoff(persists: u64, max_unwritten: u64, log: MemLog) -> (Vec<(u64, bool)>, usize) {
    let cfg = Config::new([1; 16], NodeId(0), vec![NodeId(0)], [2; 32]);
    let replica = Replica::new(cfg, [], Timestamp(0)).unwrap();
    let (events, rx) = mpsc::sync_channel(1);
    events.send(Ev::Written).unwrap();
    let snapshots = SnapshotWriter::new(&std::env::temp_dir());
    let mut n = Node::with(replica, None, snapshots, HashMap::new(), u64::MAX, events);
    n.max_unwritten = max_unwritten;
    let state = log.state.clone();
    n.start_writer(log);
    for id in 0..persists {
        n.persists.push(PendingPersist {
            id: PersistId(id),
            records: vec![Bytes::from_static(&[0; 10])],
            then: Vec::new(),
            replace: false,
        });
        n.pending = 10;
        n.flush();
    }
    let mut reports: Vec<(u64, bool)> = Vec::new();
    while !reports.iter().any(|&(id, ok)| id + 1 == persists || !ok) {
        // A lost wake-up leaves this thread blocked for good: shuttle reports a deadlock.
        let _ = rx.recv();
        reports.extend(n.written.reports().into_iter().map(|r| (r.id.0, r.ok)));
    }
    let appends = state.lock().unwrap().1;
    (reports, appends)
}

/// `QPX_SHUTTLE_PERSISTS`, or 1: two take 3 minutes to explore, so only nightly runs them.
fn persists() -> u64 {
    std::env::var("QPX_SHUTTLE_PERSISTS").ok().and_then(|s| s.parse().ok()).unwrap_or(1)
}

#[test]
fn the_handoff_reports_every_persist_in_order_under_every_schedule() {
    let count = persists();
    // One byte: each flush waits for the writer to finish the one before.
    dfs(move || {
        let (reports, appends) = handoff(count, 1, MemLog::default());
        let ids: Vec<u64> =
            reports.iter().map(|&(id, ok)| if ok { id } else { panic!("{reports:?}") }).collect();
        assert_eq!(ids, (0..count).collect::<Vec<_>>(), "each persist reported once, in order");
        assert_eq!(appends as u64, count);
    });
}

#[test]
fn a_failed_append_stops_the_writer_and_releases_a_waiting_flush() {
    dfs(|| {
        let (reports, appends) = handoff(persists(), 1, MemLog { fail_at: Some(1), ..MemLog::default() });
        assert_eq!(reports, [(0, false)], "the failure is reported and nothing after it");
        assert_eq!(appends, 1, "the writer stopped at the failure");
    });
}

/// Every write is answered, in order, and no event-loop wait outlives the writer.
fn every_write_commits(max_unwritten: u64) {
    let out = writes(2, max_unwritten, MemLog::default());
    assert!(!out.fenced);
    for (i, r) in out.replies.iter().enumerate() {
        let Some(Reply::Done(applied)) = r else { panic!("write {i} answered {r:?}") };
        assert_eq!(applied.revision, i as u64 + 1, "writes commit in order");
    }
    assert!(out.appends > 0);
}

#[test]
fn writes_commit_under_sampled_schedules() {
    // One byte: every flush waits for the writer to catch up with the one before.
    pct(2_000, || every_write_commits(1));
    pct(2_000, || every_write_commits(MAX_UNWRITTEN));
}

#[test]
fn a_failed_append_fences_the_node_under_sampled_schedules() {
    pct(2_000, || {
        // Every flush waits for the one before, so the second write is the second append.
        let out = writes(2, 1, MemLog { fail_at: Some(2), ..MemLog::default() });
        assert!(out.fenced, "the storage failure fenced the replica");
        assert_eq!(out.appends, 2, "the writer stopped at the failure");
        assert!(
            !matches!(out.replies[1], Some(Reply::Done(_))),
            "a write that never reached the log was acknowledged"
        );
    });
}

#[test]
fn concurrent_snapshot_saves_keep_the_newest() {
    dfs(|| {
        let dir = std::env::temp_dir().join(format!("kv-tokio-shuttle-snap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("snapshot.bin"));
        let w = SnapshotWriter::new(&dir);
        let older = {
            let w = w.clone();
            thread::spawn(move || w.save(10, b"through 10").unwrap())
        };
        assert!(w.save(20, b"through 20").unwrap(), "nothing newer was saved");
        older.join().unwrap();
        assert_eq!(std::fs::read(dir.join("snapshot.bin")).unwrap(), b"through 20");
    });
}

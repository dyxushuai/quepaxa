//! The apply thread: it owns the KV state, applies committed slots in the order the event
//! loop emitted them, and answers the clients waiting on them. The event loop keeps the
//! `Replica` and hands over everything that reads or changes the KV state as `Job`s, in one
//! FIFO queue: a reply follows the apply of everything handed over before it, a
//! linearizable read sees every slot decided before its barrier, and a snapshot is a cut
//! at the slot the event loop names.

use bytes::Bytes;
use quepaxa::{DropReason, Entry, NodeId, ProposalId};
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;

use crate::client::{Pending, Reply};
use crate::kv::Kv;
use crate::node::{Ev, MAX_FRAME};
use crate::ops::{self, RangeReq, Request};
use crate::storage::{SnapshotWriter, encode_snapshot};
use crate::sync::mpsc::Receiver;
use crate::sync::thread;

/// Batches the event loop may hand over before the apply thread takes them; past this, the
/// event loop waits for it, and through the event queue so do peers and clients. Each batch
/// is what one turn of the event loop produced.
pub const QUEUE: usize = 64;

pub enum Job {
    /// A proposal of this replica; `payload` is empty for a membership or leader-order change.
    Proposed {
        id: ProposalId,
        payload: Vec<u8>,
        reply: oneshot::Sender<Reply>,
    },
    Commit {
        slot: u64,
        entries: Vec<Entry>,
    },
    Committed {
        id: ProposalId,
        slot: u64,
    },
    Dropped {
        id: ProposalId,
        reason: DropReason,
    },
    /// A read from the state applied so far: for a linearizable read, the event loop hands
    /// it over once its barrier is ready, so after every slot decided before it.
    Read {
        range: RangeReq,
        reply: oneshot::Sender<Reply>,
    },
    /// A snapshot replaced the state; commits after its `through` follow.
    Install(Kv),
    /// Snapshot the state, which has now applied every slot through `through`, with the
    /// replica's `core` state taken at the same point; the result returns as
    /// `Ev::SnapshotSaved`.
    Snapshot {
        through: u64,
        core: Bytes,
    },
}

pub struct Applier {
    kv: Kv,
    pending: Pending,
    /// Entries with this origin and incarnation are this replica's own proposals.
    me: NodeId,
    incarnation: u64,
    snapshots: SnapshotWriter,
    events: Sender<Ev>,
}

impl Applier {
    pub fn new(me: NodeId, incarnation: u64, snapshots: SnapshotWriter, events: Sender<Ev>) -> Self {
        Self { kv: Kv::default(), pending: Pending::default(), me, incarnation, snapshots, events }
    }

    /// Runs until the event loop drops its end of the queue.
    pub fn run(mut self, jobs: &Receiver<Vec<Job>>) {
        while let Ok(batch) = jobs.recv() {
            for job in batch {
                self.handle(job);
            }
        }
    }

    pub fn handle(&mut self, job: Job) {
        match job {
            Job::Proposed { id, payload, reply } => self.pending.write(id, payload, reply),
            Job::Commit { slot, entries } => {
                for e in &entries {
                    // A payload that is not a request (a corrupt or foreign batch) applies as
                    // nothing on every replica alike.
                    let Some(req) = Request::decode(&e.payload) else { continue };
                    let applied = ops::apply(&mut self.kv, &req);
                    if e.origin == self.me && e.incarnation == self.incarnation {
                        self.pending.applied(slot, e.id, &e.payload, applied);
                    }
                }
            }
            Job::Committed { id, slot } => self.pending.committed(id, slot, self.kv.revision()),
            Job::Dropped { id, reason } => self.pending.dropped(id, reason),
            Job::Read { range, reply } => {
                let _ = reply.send(Reply::Done(ops::read(&self.kv, &range)));
            }
            Job::Install(kv) => self.kv = kv,
            Job::Snapshot { through, core } => self.snapshot(through, core),
        }
    }

    /// Takes a snapshot (only the shard pointers are copied here) and encodes and writes it
    /// on its own thread, so neither apply nor consensus waits for the fsync. Snapshots too
    /// large for one peer frame are not saved (the log keeps growing instead), so a lagging
    /// replica can always fetch the latest one.
    fn snapshot(&mut self, through: u64, core: Bytes) {
        let kv = self.kv.snapshot();
        let (writer, events) = (self.snapshots.clone(), self.events.clone());
        thread::spawn(move || {
            let bytes = Bytes::from(encode_snapshot(through, &core, &kv));
            // Unshare the shards now: until then writes to them wait in overlays.
            drop(kv);
            let saved = if bytes.len() + 1 > MAX_FRAME {
                Err(format!("{} bytes exceed the peer frame limit", bytes.len()))
            } else {
                writer.save(through, &bytes).map_err(|e| e.to_string())
            };
            let _ = events.blocking_send(Ev::SnapshotSaved { through, core, bytes, saved });
        });
    }

    #[cfg(all(test, not(feature = "shuttle")))]
    pub fn kv(&self) -> &Kv {
        &self.kv
    }
}

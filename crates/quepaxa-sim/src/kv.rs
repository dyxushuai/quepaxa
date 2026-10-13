//! A toy key-value store over the log, and a linearizability oracle for it.
//!
//! Every invocation and completion gets the next value of one counter as it happens, so
//! real-time order is exact even when several events share a virtual timestamp.
//!
//! Writes carry unique values (`k<key>:<tag>`), so the log gives every write a position
//! and the store at any slot is fully determined. A read (via a read barrier) is
//! linearizable iff the write it returns:
//! 1. was invoked before the read completed,
//! 2. is not older (in log order) than any write to that key completed before the read
//!    was invoked, and
//! 3. is not older than what any read of that key completed before this read was invoked
//!    returned.

use std::collections::{BTreeMap, HashMap};

use bytes::Bytes;
use quepaxa::ProposalId;

/// Number of keys; few keys mean much contention.
pub(crate) const KEYS: u64 = 3;

pub(crate) fn key_of(payload: &[u8]) -> u8 {
    payload.get(1).copied().unwrap_or(b'0')
}

pub(crate) fn write_payload(key: u64, tag: &str) -> Bytes {
    Bytes::from(format!("k{key}:{tag}"))
}

struct WriteOp {
    invoked: u64,
    completed: Option<u64>,
}

struct ReadOp {
    key: u8,
    invoked: u64,
    completed: u64,
    value: Option<Bytes>,
}

/// One replica's applied store.
pub(crate) type State = BTreeMap<u8, Bytes>;

#[derive(Default)]
pub(crate) struct Kv {
    writes: HashMap<Bytes, WriteOp>,
    /// Log position (slot, index) of every committed write.
    positions: HashMap<Bytes, (u64, usize)>,
    reads: Vec<ReadOp>,
    /// Per node: barrier id → (key, invoked).
    pending_reads: Vec<HashMap<ProposalId, (u8, u64)>>,
    /// Per node: applied state.
    state: Vec<State>,
    /// Event ordinal: real-time order of invocations and completions.
    clock: u64,
}

impl Kv {
    pub fn new(nodes: usize) -> Self {
        Self {
            pending_reads: vec![HashMap::new(); nodes],
            state: vec![BTreeMap::new(); nodes],
            ..Self::default()
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    pub fn write_invoked(&mut self, payload: Bytes) {
        let at = self.tick();
        self.writes.insert(payload, WriteOp { invoked: at, completed: None });
    }

    pub fn write_completed(&mut self, payload: &Bytes) {
        let at = self.tick();
        if let Some(w) = self.writes.get_mut(payload) {
            w.completed.get_or_insert(at);
        }
    }

    pub fn read_invoked(&mut self, node: usize, id: ProposalId, key: u8) {
        let at = self.tick();
        self.pending_reads[node].insert(id, (key, at));
    }

    pub fn no_pending_reads(&self) -> bool {
        self.pending_reads.iter().all(HashMap::is_empty)
    }

    /// The node applied every commit up to the barrier's slot: read its state now.
    pub fn read_ready(&mut self, node: usize, id: ProposalId) {
        let now = self.tick();
        if let Some((key, invoked)) = self.pending_reads[node].remove(&id) {
            let value = self.state[node].get(&key).cloned();
            self.reads.push(ReadOp { key, invoked, completed: now, value });
        }
    }

    pub fn read_dropped(&mut self, node: usize, id: ProposalId) {
        self.pending_reads[node].remove(&id);
    }

    /// The node restarted: its pending reads are gone and it replays its state.
    pub fn restarted(&mut self, node: usize) {
        self.pending_reads[node].clear();
        self.state[node].clear();
    }

    pub fn first_commit(&mut self, slot: u64, payloads: &[Bytes]) {
        for (i, p) in payloads.iter().enumerate() {
            self.positions.insert(p.clone(), (slot, i));
        }
    }

    pub fn apply(&mut self, node: usize, payloads: &[Bytes]) {
        for p in payloads {
            self.state[node].insert(key_of(p), p.clone());
        }
    }

    pub fn state(&self, node: usize) -> State {
        self.state[node].clone()
    }

    pub fn set_state(&mut self, node: usize, state: State) {
        self.state[node] = state;
    }

    pub fn reads(&self) -> usize {
        self.reads.len()
    }

    fn pos(&self, v: &Option<Bytes>) -> Option<(u64, usize)> {
        v.as_ref().and_then(|v| self.positions.get(v).copied())
    }

    /// Checks every completed read; returns a description of the first violation.
    pub fn check(&self) -> Result<(), String> {
        for r in &self.reads {
            let seen = self.pos(&r.value);
            if let Some(v) = &r.value {
                let w = &self.writes[v];
                if w.invoked > r.completed {
                    return Err(format!("read of {v:?} completed before the write was invoked"));
                }
            }
            for (p, w) in &self.writes {
                let completed_before = w.completed.is_some_and(|c| c < r.invoked);
                if key_of(p) == r.key && completed_before && self.positions.get(p).copied() > seen {
                    return Err(format!(
                        "stale read: {:?} returned although {p:?} completed before it",
                        r.value
                    ));
                }
            }
            for earlier in &self.reads {
                if earlier.key == r.key && earlier.completed < r.invoked && self.pos(&earlier.value) > seen {
                    return Err(format!("read went backwards: {:?} after {:?}", r.value, earlier.value));
                }
            }
        }
        Ok(())
    }
}

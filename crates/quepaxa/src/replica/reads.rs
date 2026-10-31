//! Linearizable reads without a log slot (`verification/tla/Reads.tla`).
//!
//! A write completes only once its slot is decided, and a slot is decided only on record
//! replies from a majority of its members, each sent after the reply's register was
//! durable. So every completed write is a slot that a majority of its members hold state
//! for, and a read that hears from a majority learns a bound past it.
//!
//! Recorder side: a replica answers a read query with its high-water mark, the first slot
//! past every slot it holds state for: registers, decided slots, and the snapshot floor
//! (registers go once their decisions are durable, decided slots once compacted). It
//! answers from memory, without waiting for pending writes: what is in memory is never
//! below what is durable, and a mark that is too high only makes the reader wait for a
//! slot that takeover then decides.

use crate::codec::Body;
use crate::types::{NodeId, Slot};

use super::Replica;

impl Replica {
    /// The first slot past every slot this replica holds state for.
    pub(super) fn high_water(&self) -> u64 {
        let past = |s: Option<&Slot>| s.map_or(0, |s| s.0.saturating_add(1));
        past(self.registers.last_key_value().map(|(s, _)| s))
            .max(past(self.log.last_key_value().map(|(s, _)| s)))
            .max(self.floor)
    }

    pub(super) fn on_read_query(&mut self, from: NodeId, round: u64) {
        let high = Slot(self.high_water());
        self.send(from, Body::ReadReply { round, high });
    }
}

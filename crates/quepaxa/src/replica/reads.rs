//! Linearizable reads without a log slot (`verification/tla/Reads.tla`, proved for fixed
//! membership in `ReadsProof.tla`).
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
//!
//! Reader side: reads requested while no round is in flight start one; others wait for the
//! next. A round asks the members of every epoch in force from the decided prefix on
//! (pending ones included) and takes K, the largest mark answered (at least 1, so the first
//! read of a fresh cluster waits for slot 0). Its reads are ready once the decided prefix,
//! all applied, reaches K and a majority of each of those member sets answered. Learning
//! slots may teach the replica a new epoch; its members are asked in the same round, sent
//! after every read in it. Slots below the prefix are applied; an epoch the replica does not
//! know cannot hold a write completed before the read: the change that started it was
//! decided in a known epoch, which a majority answered, so K would be past it.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::mem;

use crate::api::{Action, DropReason, Error};
use crate::codec::Body;
use crate::types::{NodeId, ProposalId, Slot, Timestamp};

use super::Replica;

/// Reads waiting for a round, and the round in flight.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "model", derive(Hash))]
pub(super) struct Reads {
    queued: Vec<ProposalId>,
    round: Option<Round>,
    next_round: u64,
}

impl Reads {
    /// Rounds are numbered from the incarnation up, so a late answer to a round asked
    /// before a restart never matches one asked after it.
    pub(super) fn new(incarnation: u64) -> Self {
        Self { queued: Vec::new(), round: None, next_round: incarnation << 32 }
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "model", derive(Hash))]
struct Round {
    number: u64,
    reads: Vec<ProposalId>,
    asked: BTreeSet<NodeId>,
    /// Marks answered, by replica.
    answers: BTreeMap<NodeId, u64>,
    /// When to ask again those that have not answered.
    resend_at: Timestamp,
}

impl Reads {
    pub(super) fn len(&self) -> usize {
        self.queued.len() + self.round.as_ref().map_or(0, |r| r.reads.len())
    }

    pub(super) fn resend_at(&self) -> Option<Timestamp> {
        self.round.as_ref().map(|r| r.resend_at)
    }
}

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

    pub(super) fn on_read_reply(&mut self, from: NodeId, round: u64, high: Slot) {
        if let Some(r) = self.reads.round.as_mut().filter(|r| r.number == round) {
            let mark = r.answers.entry(from).or_default();
            *mark = (*mark).max(high.0);
        }
    }

    /// Queues a read for the next round; the caller pumps it.
    pub(super) fn submit_read(&mut self) -> Result<ProposalId, Error> {
        if self.fenced {
            return Err(Error::Fenced);
        }
        if self.outstanding.len() + self.reads.len() >= self.cfg.max_outstanding_proposals as usize {
            return Err(Error::Busy);
        }
        let id = ProposalId(self.next_proposal);
        self.next_proposal += 1;
        self.reads.queued.push(id);
        Ok(id)
    }

    /// Member sets of every epoch from the decided prefix on, without repeats.
    fn read_member_sets(&self) -> Vec<Vec<NodeId>> {
        let mut sets: Vec<Vec<NodeId>> = self
            .schedule
            .member_sets_from(Slot(self.committed))
            .map(|order| {
                let mut set = order.to_vec();
                set.sort_unstable();
                set
            })
            .collect();
        sets.sort_unstable();
        sets.dedup();
        sets
    }

    fn ask(&mut self, number: u64, members: impl IntoIterator<Item = NodeId>) {
        for m in members {
            self.send(m, Body::ReadQuery { round: number });
        }
    }

    /// Starts a round for the queued reads, finishes the round in flight once it is ready,
    /// and makes progress toward it otherwise. Run after the inbox is drained, so a ready
    /// read follows the commits it covers.
    pub(super) fn check_reads(&mut self) {
        if self.fenced {
            return;
        }
        loop {
            let Some(round) = &self.reads.round else {
                if self.reads.queued.is_empty() {
                    return;
                }
                self.start_round();
                continue;
            };
            let k = round.answers.values().copied().max().unwrap_or(0).max(1);
            if self.committed < k || self.emitted < self.committed {
                self.wait_for(k);
                return;
            }
            let sets = self.read_member_sets();
            let short: Vec<&Vec<NodeId>> = sets
                .iter()
                .filter(|set| 2 * set.iter().filter(|m| round.answers.contains_key(m)).count() <= set.len())
                .collect();
            if !short.is_empty() {
                let new: BTreeSet<NodeId> = short
                    .iter()
                    .flat_map(|s| s.iter().copied())
                    .filter(|m| !round.asked.contains(m))
                    .collect();
                if !new.is_empty() {
                    let number = round.number;
                    if let Some(r) = self.reads.round.as_mut() {
                        r.asked.extend(new.iter().copied());
                    }
                    self.ask(number, new);
                }
                return;
            }
            let Some(round) = self.reads.round.take() else { return };
            for id in round.reads {
                self.actions.push(Action::ReadReady { id, slot: Slot(self.emitted - 1) });
            }
        }
    }

    fn start_round(&mut self) {
        let number = self.reads.next_round;
        self.reads.next_round += 1;
        let asked: BTreeSet<NodeId> = self.read_member_sets().into_iter().flatten().collect();
        let reads = mem::take(&mut self.reads.queued);
        let resend_at = self.now.after(self.cfg.retransmit_interval);
        self.reads.round =
            Some(Round { number, reads, asked: asked.clone(), answers: BTreeMap::new(), resend_at });
        self.ask(number, asked);
    }

    /// Below K: the next slot is driven if nobody else does (on a fresh cluster nobody has
    /// recorded it). Later ones are driven by the recorders that recorded them, and a
    /// replica that decided them says so in every envelope, which starts a catch-up; driving
    /// them from here would only contend with their proposers.
    fn wait_for(&mut self, k: u64) {
        if self.committed < k {
            self.seen.entry(Slot(self.committed)).or_insert(self.now);
        }
    }

    /// Asks again the members that have not answered the round in flight.
    pub(super) fn resend_reads(&mut self) {
        let Some(round) = self.reads.round.as_mut().filter(|r| r.resend_at <= self.now) else { return };
        round.resend_at = self.now.after(self.cfg.retransmit_interval);
        let number = round.number;
        let silent: Vec<NodeId> =
            round.asked.iter().copied().filter(|m| !round.answers.contains_key(m)).collect();
        self.ask(number, silent);
    }

    /// Fencing drops every pending read.
    pub(super) fn drop_reads(&mut self) {
        let queued = mem::take(&mut self.reads.queued);
        let ids = queued.into_iter().chain(self.reads.round.take().into_iter().flat_map(|r| r.reads));
        for id in ids {
            self.actions.push(Action::Dropped { id, reason: DropReason::Fenced });
        }
    }
}

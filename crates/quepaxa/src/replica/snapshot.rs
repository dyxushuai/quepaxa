//! Compaction and snapshot installation (spec §5).
//!
//! The application owns snapshots of its state; the replica contributes `core_state`
//! (commit dedup and leader schedule as of the last emitted commit), which must travel
//! with the application snapshot. After `compact` or `install_snapshot` the replica emits
//! a checkpoint: a `Persist` that replaces all earlier records.

use alloc::vec::Vec;
use bytes::Bytes;

use super::Replica;
use crate::api::{Action, Error};
use crate::core_state::CoreState;
use crate::durable::Record;
use crate::types::{NodeId, Slot, Timestamp};

impl Replica {
    /// Number of slots already returned as [`Action::Commit`] or covered by an installed
    /// snapshot: the application state corresponds to this prefix.
    pub fn emitted_index(&self) -> u64 {
        self.emitted
    }

    /// Replica state that must be stored with an application snapshot. It covers exactly
    /// the commits already returned by [`Replica::poll_actions`]; take it after applying them.
    pub fn core_state(&self) -> Bytes {
        CoreState { next: self.emitted, schedule: self.applied_schedule.clone(), dedup: self.dedup.clone() }
            .encode()
    }

    /// Discards log state up to and including `through`, which the application has
    /// durably snapshotted together with `core_state` (taken right after applying slot
    /// `through`). Older or repeated compactions are ignored.
    pub fn compact(&mut self, now: Timestamp, through: Slot, core_state: Bytes) -> Result<(), Error> {
        self.begin(now)?;
        if self.fenced {
            return Err(Error::Fenced);
        }
        let state = self.check_state(through, &core_state)?;
        if state.next > self.emitted {
            return Err(Error::InvalidCoreState);
        }
        if state.next > self.floor {
            self.raise_floor(state.next, core_state);
        }
        self.flush();
        Ok(())
    }

    /// Adopts a snapshot covering slots up to `through`: a peer's, after
    /// [`Action::SnapshotNeeded`], or the application's own after a restart (its snapshot
    /// may be newer than the replica's last durable compaction).
    ///
    /// Returns `Ok(false)` without changing anything when this replica already emitted every
    /// slot the snapshot covers (it went stale in transit); replace the application state
    /// only on `Ok(true)`, so the application is never rewound.
    ///
    /// On `Ok(true)` the replica queues a checkpoint `Persist` that discards the log below
    /// the snapshot. Store the application snapshot durably before executing that persist
    /// (and fail it with [`crate::Input::StorageFailed`] if you cannot): otherwise a crash
    /// in between leaves neither the log nor a snapshot covering those slots.
    pub fn install_snapshot(
        &mut self,
        now: Timestamp,
        through: Slot,
        core_state: Bytes,
    ) -> Result<bool, Error> {
        self.begin(now)?;
        if self.fenced {
            return Err(Error::Fenced);
        }
        let state = self.check_state(through, &core_state)?;
        if state.next <= self.emitted {
            return Ok(false);
        }
        self.applied_schedule = state.schedule.clone();
        self.schedule = state.schedule;
        self.dedup = state.dedup;
        self.emitted = state.next;
        self.committed = state.next;
        // Local work the snapshot already contains is resolved from its dedup state.
        let me = self.me;
        let inc = self.incarnation;
        let done: Vec<_> = self
            .outstanding
            .iter()
            .filter(|(id, _)| self.dedup.contains(me, inc, **id))
            .map(|(id, (_, barrier))| (*id, *barrier))
            .collect();
        for (id, barrier) in done {
            self.outstanding.remove(&id);
            self.forwarded.remove(&id);
            self.actions.push(if barrier {
                Action::ReadReady { id, slot: through }
            } else {
                Action::Committed { id, slot: through }
            });
        }
        let stale: Vec<Slot> = self.proposers.range(..Slot(state.next)).map(|(s, _)| *s).collect();
        for slot in stale {
            if let Some(a) = self.proposers.remove(&slot)
                && let Some(b) = self.batches.get_mut(&a.batch)
            {
                b.slot = None;
            }
        }
        self.raise_floor(state.next, core_state);
        self.advance_committed();
        self.emit_commits();
        // Resume fetching whatever the cluster decided after the snapshot.
        self.catch_up = None;
        if self.peer_committed > self.committed
            && let Some(peer) = self.ahead_peer
        {
            self.request_catch_up(peer);
        }
        self.pump();
        self.flush();
        Ok(true)
    }

    fn check_state(&self, through: Slot, core_state: &Bytes) -> Result<CoreState, Error> {
        decode_state(through, core_state, &self.cfg.members).ok_or(Error::InvalidCoreState)
    }

    fn raise_floor(&mut self, next: u64, core_state: Bytes) {
        let floor = Slot(next);
        self.floor = next;
        self.log = self.log.split_off(&floor);
        self.registers = self.registers.split_off(&floor);
        self.seen = self.seen.split_off(&floor);
        #[cfg(feature = "model")]
        {
            self.decided_steps = self.decided_steps.split_off(&floor);
        }
        self.next_alloc = self.next_alloc.max(next);
        self.snapshot_state = Some(core_state);
        self.checkpoint();
    }

    /// Rewrites the durable state from scratch: everything recovery needs, nothing below the floor.
    fn checkpoint(&mut self) {
        let mut records = alloc::vec![Record::Incarnation(self.incarnation).encode()];
        let reserved = self.reserved_through.or(self.h_floor.checked_sub(1));
        if let Some(t) = reserved {
            records.push(Record::Reservation { through: Slot(t) }.encode());
        }
        if let Some(state) = &self.snapshot_state {
            records.push(
                Record::Compacted { through: Slot(self.floor - 1), core_state: state.clone() }.encode(),
            );
        }
        records.extend(
            self.registers
                .iter()
                .map(|(slot, reg)| Record::Recorder { slot: *slot, reg: reg.clone() }.encode()),
        );
        records.extend(
            self.log
                .iter()
                .map(|(slot, d)| Record::Decided { slot: *slot, proposal: d.proposal.clone() }.encode()),
        );
        // Writes made earlier in this call are already reflected in the checkpoint.
        self.gate.checkpoint(records);
    }

    /// A peer no longer has slots we need.
    pub(super) fn on_compacted(&mut self, from: NodeId, through: Slot) {
        if through.0 < self.committed {
            return;
        }
        let quiet = self.snapshot_asked.is_none_or(|t| t.after(self.cfg.hedge_max) <= self.now);
        if quiet {
            self.snapshot_asked = Some(self.now);
            self.actions.push(Action::SnapshotNeeded { peer: from, through });
        }
    }
}

/// Core state covering every slot through `through`, if `bytes` is one.
pub(super) fn decode_state(through: Slot, bytes: &Bytes, members: &[NodeId]) -> Option<CoreState> {
    let state = CoreState::decode(bytes, members).ok()?;
    (through.0.checked_add(1) == Some(state.next) && state.next <= crate::types::MAX_SLOT).then_some(state)
}

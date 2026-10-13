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
    /// Replica state that must be stored with an application snapshot. It covers exactly
    /// the commits already returned by [`Replica::poll_actions`]; take it after applying them.
    pub fn core_state(&self) -> Bytes {
        CoreState { next: self.emitted, schedule: self.schedule.clone(), dedup: self.dedup.clone() }.encode()
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

    /// Adopts a snapshot covering slots up to `through` after the application installed
    /// the matching application state: a peer's, after [`Action::SnapshotNeeded`], or the
    /// application's own after a restart (its snapshot may be newer than the replica's
    /// last durable compaction). Snapshots this replica is already past are ignored.
    pub fn install_snapshot(
        &mut self,
        now: Timestamp,
        through: Slot,
        core_state: Bytes,
    ) -> Result<(), Error> {
        self.begin(now)?;
        if self.fenced {
            return Err(Error::Fenced);
        }
        let state = self.check_state(through, &core_state)?;
        if state.next <= self.emitted {
            return Ok(());
        }
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
        self.pump();
        self.flush();
        Ok(())
    }

    fn check_state(&self, through: Slot, core_state: &Bytes) -> Result<CoreState, Error> {
        let state = CoreState::decode(core_state, &self.cfg.members).map_err(|_| Error::InvalidCoreState)?;
        if through.0.checked_add(1) != Some(state.next) {
            return Err(Error::InvalidCoreState);
        }
        Ok(state)
    }

    fn raise_floor(&mut self, next: u64, core_state: Bytes) {
        let floor = Slot(next);
        self.floor = next;
        self.log = self.log.split_off(&floor);
        self.registers = self.registers.split_off(&floor);
        self.seen = self.seen.split_off(&floor);
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
        self.writes = records;
        self.replace_next = true;
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

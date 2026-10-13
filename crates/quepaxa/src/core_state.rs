//! The replica state an application snapshot must carry so that a replica restored from
//! it filters and leads exactly like one that applied the whole log.

use alloc::vec::Vec;
use bytes::{BufMut, Bytes};

use crate::codec::{DecodeError, Reader};
use crate::dedup::Dedup;
use crate::schedule::Schedule;
use crate::types::NodeId;

/// 2: the schedule is the one as of `next` (version 1 stored the decided-prefix schedule).
const VERSION: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CoreState {
    /// First slot the state does not cover.
    pub next: u64,
    pub schedule: Schedule,
    pub dedup: Dedup,
}

impl CoreState {
    pub fn encode(&self) -> Bytes {
        let mut out = Vec::new();
        out.put_u8(VERSION);
        out.put_u64_le(self.next);
        self.schedule.encode(&mut out);
        self.dedup.encode(&mut out);
        Bytes::from(out)
    }

    pub fn decode(buf: &Bytes, members: &[NodeId]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(buf);
        if r.u8()? != VERSION {
            return Err(DecodeError::BadVersion);
        }
        let next = r.u64()?;
        let schedule = Schedule::decode(&mut r, members)?;
        let dedup = Dedup::decode(&mut r)?;
        r.finish()?;
        Ok(Self { next, schedule, dedup })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ProposalId, Slot};

    fn members() -> Vec<NodeId> {
        (0..3).map(NodeId).collect()
    }

    #[test]
    fn round_trips_after_rotations_and_dedup_activity() {
        let mut schedule = Schedule::new(&members());
        for slot in 0..20 {
            schedule.observe(Slot(slot), NodeId(2));
        }
        let mut dedup = Dedup::default();
        for id in [0, 1, 5, 9] {
            dedup.first_time(NodeId(1), 3, ProposalId(id));
        }
        let state = CoreState { next: 20, schedule, dedup };
        assert_eq!(CoreState::decode(&state.encode(), &members()), Ok(state));
    }

    #[test]
    fn rejects_a_schedule_naming_strangers() {
        let state = CoreState { next: 1, schedule: Schedule::new(&members()), dedup: Dedup::default() };
        let others: Vec<NodeId> = (5..8).map(NodeId).collect();
        assert!(CoreState::decode(&state.encode(), &others).is_err());
    }

    #[test]
    fn decode_never_panics() {
        bolero::check!().for_each(|input: &[u8]| {
            let _ = CoreState::decode(&Bytes::copy_from_slice(input), &members());
        });
    }
}

//! The replica state an application snapshot must carry so that a replica restored from
//! it filters and leads exactly like one that applied the whole log.

use alloc::vec::Vec;
use bytes::{BufMut, Bytes};

use crate::codec::{DecodeError, Reader};
use crate::dedup::Dedup;
use crate::schedule::Schedule;
use crate::types::NodeId;

/// Only this version is read; earlier ones are rejected (no compatibility before 1.0).
const VERSION: u8 = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CoreState {
    /// First slot the state does not cover.
    pub next: u64,
    /// `Config::leaders` of the replica that built the state; a replica configured with
    /// another order must not adopt it.
    pub leaders: Vec<NodeId>,
    pub schedule: Schedule,
    pub dedup: Dedup,
}

impl CoreState {
    pub fn encode(&self) -> Bytes {
        let mut out = Vec::new();
        out.put_u8(VERSION);
        out.put_u64_le(self.next);
        out.put_u32_le(self.leaders.len() as u32);
        for n in &self.leaders {
            out.put_u32_le(n.0);
        }
        self.schedule.encode(&mut out);
        self.dedup.encode(&mut out);
        Bytes::from(out)
    }

    pub fn decode(buf: &Bytes, members: &[NodeId]) -> Result<Self, DecodeError> {
        let mut r = Reader::new(buf);
        let version = r.u8()?;
        if version != VERSION {
            return Err(DecodeError::BadVersion);
        }
        let next = r.u64()?;
        let mut by_id = members.to_vec();
        by_id.sort_unstable();
        let n = r.u32()? as usize;
        if n != members.len() {
            return Err(DecodeError::BadTag);
        }
        let leaders: Vec<NodeId> = (0..n).map(|_| r.u32().map(NodeId)).collect::<Result<_, _>>()?;
        let mut sorted = leaders.clone();
        sorted.sort_unstable();
        if sorted != by_id {
            return Err(DecodeError::BadTag);
        }
        let schedule = Schedule::decode(&mut r, members)?;
        if !schedule.fits(next) {
            return Err(DecodeError::BadTag);
        }
        let dedup = Dedup::decode(&mut r)?;
        r.finish()?;
        Ok(Self { next, leaders, schedule, dedup })
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
        let state =
            CoreState { next: 20, leaders: alloc::vec![NodeId(2), NodeId(0), NodeId(1)], schedule, dedup };
        assert_eq!(CoreState::decode(&state.encode(), &members()), Ok(state));
    }

    #[test]
    fn rejects_a_schedule_ahead_of_its_snapshot() {
        let mut schedule = Schedule::new(&members());
        for slot in 0..20 {
            schedule.observe(Slot(slot), NodeId(2));
        }
        let state = |next| CoreState {
            next,
            leaders: members(),
            schedule: schedule.clone(),
            dedup: Dedup::default(),
        };
        // A rotation counted at slot 7 starts at 7 + LAG: not yet decided by slot 5.
        assert!(CoreState::decode(&state(20).encode(), &members()).is_ok());
        assert_eq!(CoreState::decode(&state(5).encode(), &members()), Err(DecodeError::BadTag));
    }

    #[test]
    fn rejects_a_schedule_naming_strangers() {
        let state = CoreState {
            next: 1,
            leaders: members(),
            schedule: Schedule::new(&members()),
            dedup: Dedup::default(),
        };
        let others: Vec<NodeId> = (5..8).map(NodeId).collect();
        assert!(CoreState::decode(&state.encode(), &others).is_err());
    }

    #[test]
    fn decode_never_panics() {
        bolero::check!().for_each(|input: &[u8]| {
            let _ = CoreState::decode(&Bytes::copy_from_slice(input), &members());
        });
    }

    #[test]
    fn earlier_versions_are_rejected() {
        let state = CoreState {
            next: 4,
            leaders: members(),
            schedule: Schedule::new(&members()),
            dedup: Dedup::default(),
        };
        let mut old = state.encode().to_vec();
        old[0] = VERSION - 1;
        assert_eq!(CoreState::decode(&Bytes::from(old), &members()), Err(DecodeError::BadVersion));
    }

    #[test]
    fn a_leader_order_must_be_a_permutation_of_the_members() {
        for leaders in [alloc::vec![NodeId(0), NodeId(0), NodeId(1)], alloc::vec![NodeId(0), NodeId(1)]] {
            let state =
                CoreState { next: 1, leaders, schedule: Schedule::new(&members()), dedup: Dedup::default() };
            assert!(CoreState::decode(&state.encode(), &members()).is_err());
        }
    }
}

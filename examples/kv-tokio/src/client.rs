//! What clients ask the event loop for, what they get back, and how a committed request's
//! result finds the client waiting for it. Both front ends (lines and etcd gRPC) use these.

use std::collections::HashMap;

use quepaxa::{NodeId, ProposalId};
use tokio::sync::oneshot;

use crate::ops::{Applied, RangeReq, Request, Response};

pub enum Cmd {
    /// A request that goes through the log (put, delete range, transaction).
    Write(Request),
    /// A range: through a read barrier when linearizable, else from local state.
    Read { range: RangeReq, linearizable: bool },
    /// The replica's counters, on one line.
    Stats,
    /// New members, preferred leaders first (empty: by id).
    Reconfigure(Vec<NodeId>, Vec<NodeId>),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    Done(Applied),
    /// The request committed but its result is not known here: an installed snapshot
    /// resolved it. `revision` is the store revision when the reply was made.
    Unknown {
        revision: u64,
    },
    Text(String),
    Failed(Failure),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// The replica refused the request; nothing was proposed.
    Refused(quepaxa::Error),
    /// The replica fenced before the outcome was known: it may or may not commit.
    OutcomeUnknown,
    /// The replica fenced; the read was not served.
    Fenced,
}

/// The line protocol's reply to a request.
pub fn line(reply: Reply) -> String {
    match reply {
        Reply::Done(Applied { response: Response::Range { kvs, .. }, .. }) => match kvs.first() {
            Some((_, e)) => format!("VALUE {}", String::from_utf8_lossy(&e.value)),
            None => "NONE".to_owned(),
        },
        Reply::Done(_) | Reply::Unknown { .. } => "OK".to_owned(),
        Reply::Text(t) => t,
        Reply::Failed(Failure::Refused(e)) => format!("ERR {e}"),
        Reply::Failed(Failure::OutcomeUnknown) => "ERR outcome unknown (replica fenced)".to_owned(),
        Reply::Failed(Failure::Fenced) => "ERR replica fenced".to_owned(),
    }
}

/// Proposals this replica made and is waiting on.
#[derive(Default)]
pub struct Pending {
    /// The proposal's payload (empty for a reconfiguration) and its client.
    writes: HashMap<ProposalId, (Vec<u8>, oneshot::Sender<Reply>)>,
    /// Results of this replica's own committed entries, by id: the slot they committed in.
    results: HashMap<ProposalId, Vec<(u64, Applied)>>,
}

impl Pending {
    pub fn write(&mut self, id: ProposalId, payload: Vec<u8>, reply: oneshot::Sender<Reply>) {
        self.writes.insert(id, (payload, reply));
    }

    /// An entry this replica originated was applied in `slot`. Its result is kept only if a
    /// client waits on that id with the same payload: an earlier incarnation's entry decided
    /// late can carry an id that is pending again.
    pub fn applied(&mut self, slot: u64, id: ProposalId, payload: &[u8], applied: Applied) {
        if self.writes.get(&id).is_some_and(|(p, _)| p == payload) {
            self.results.entry(id).or_default().push((slot, applied));
        }
    }

    /// `id` committed in `slot` (or, after a snapshot, at or before it): answers its client.
    pub fn committed(&mut self, id: ProposalId, slot: u64, revision: u64) {
        let found = self.results.remove(&id).and_then(|rs| rs.into_iter().find(|(s, _)| *s == slot));
        if let Some((payload, reply)) = self.writes.remove(&id) {
            let r = match found {
                Some((_, applied)) => Reply::Done(applied),
                None if payload.is_empty() => Reply::Text("OK".to_owned()),
                None => Reply::Unknown { revision },
            };
            let _ = reply.send(r);
        }
    }

    pub fn dropped(&mut self, id: ProposalId) {
        self.results.remove(&id);
        if let Some((_, reply)) = self.writes.remove(&id) {
            let _ = reply.send(Reply::Failed(Failure::OutcomeUnknown));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::Response;

    fn put_result(revision: u64) -> Applied {
        Applied { revision, response: Response::Put { prev: None } }
    }

    fn waiting(p: &mut Pending, id: u64, payload: &[u8]) -> oneshot::Receiver<Reply> {
        let (tx, rx) = oneshot::channel();
        p.write(ProposalId(id), payload.to_vec(), tx);
        rx
    }

    #[test]
    fn a_committed_request_gets_the_result_of_its_own_slot() {
        let mut p = Pending::default();
        let mut rx = waiting(&mut p, 1, b"req");
        p.applied(7, ProposalId(1), b"req", put_result(3));
        p.committed(ProposalId(1), 7, 3);
        assert_eq!(rx.try_recv(), Ok(Reply::Done(put_result(3))));
    }

    #[test]
    fn a_late_entry_of_an_earlier_incarnation_does_not_answer_a_new_proposal() {
        let mut p = Pending::default();
        let mut rx = waiting(&mut p, 1, b"new");
        // The old incarnation's id 1 carried another request.
        p.applied(5, ProposalId(1), b"old", put_result(2));
        p.applied(6, ProposalId(1), b"new", put_result(3));
        p.committed(ProposalId(1), 6, 3);
        assert_eq!(rx.try_recv(), Ok(Reply::Done(put_result(3))));
    }

    #[test]
    fn a_proposal_resolved_by_a_snapshot_has_an_unknown_result() {
        let mut p = Pending::default();
        let mut rx = waiting(&mut p, 4, b"txn");
        p.committed(ProposalId(4), 90, 12);
        assert_eq!(rx.try_recv(), Ok(Reply::Unknown { revision: 12 }));
        let mut reconf = waiting(&mut p, 5, b"");
        p.committed(ProposalId(5), 91, 12);
        assert_eq!(reconf.try_recv(), Ok(Reply::Text("OK".to_owned())));
    }

    #[test]
    fn a_dropped_proposal_reports_an_unknown_outcome_and_leaves_nothing_behind() {
        let mut p = Pending::default();
        let mut rx = waiting(&mut p, 2, b"req");
        p.applied(3, ProposalId(2), b"req", put_result(1));
        p.dropped(ProposalId(2));
        assert_eq!(rx.try_recv(), Ok(Reply::Failed(Failure::OutcomeUnknown)));
        assert!(p.writes.is_empty() && p.results.is_empty());
    }

    #[test]
    fn lines_keep_the_old_protocol() {
        use crate::kv::Entry;
        let value = Entry { value: b"v".as_slice().into(), create: 1, modified: 1, version: 1 };
        let range = |kvs| {
            Reply::Done(Applied { revision: 1, response: Response::Range { kvs, more: false, count: 0 } })
        };
        assert_eq!(line(range(vec![(b"k".as_slice().into(), value)])), "VALUE v");
        assert_eq!(line(range(Vec::new())), "NONE");
        assert_eq!(line(Reply::Done(put_result(1))), "OK");
        assert_eq!(line(Reply::Unknown { revision: 1 }), "OK");
        assert_eq!(
            line(Reply::Failed(Failure::Refused(quepaxa::Error::Busy))),
            format!("ERR {}", quepaxa::Error::Busy)
        );
    }
}

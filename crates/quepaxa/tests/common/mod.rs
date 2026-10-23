//! Helpers shared by the replica API tests.

#![allow(dead_code)] // each test binary uses a different part

use bytes::Bytes;
use quepaxa::{Action, Config, Input, NodeId, Replica, Timestamp};

pub const T0: Timestamp = Timestamp(0);

pub fn config(members: &[u32]) -> Config {
    Config::new([9; 16], NodeId(members[0]), members.iter().copied().map(NodeId).collect(), [1; 32])
}

pub fn drain(r: &mut Replica) -> Vec<Action> {
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    out
}

/// Acknowledges every persist until the replica goes quiet; returns all other actions
/// and the durable records written.
pub fn settle(r: &mut Replica, durable: &mut Vec<Bytes>) -> Vec<Action> {
    let mut seen = Vec::new();
    let mut pending = drain(r);
    while !pending.is_empty() {
        let mut acks = Vec::new();
        for a in pending {
            match a {
                Action::Persist { id, records, replace, .. } => {
                    if replace {
                        durable.clear();
                    }
                    durable.extend(records);
                    acks.push(id);
                }
                other => seen.push(other),
            }
        }
        pending = Vec::new();
        for id in acks {
            r.handle(T0, Input::Persisted(id)).unwrap();
            pending.extend(drain(r));
        }
    }
    seen
}

pub fn ordered(id: u32, order: &[u32]) -> Replica {
    let mut cfg = config(&[0, 1, 2]);
    cfg.id = NodeId(id);
    cfg.leader_order = order.iter().copied().map(NodeId).collect();
    Replica::new(cfg, [], T0).unwrap()
}

//! Leader placement decisions, from injected round trips and write counts.

mod exploration;
mod membership;
mod moves;

use super::placement::PROBE_INTERVAL;
use super::*;
use crate::placement_policy::{Exclusion, Led, Periods, Target, WRITE_FLOOR};

const MS: u64 = 1_000_000;
const SUSTAIN: u64 = 180 * PROBE_INTERVAL;
const HOLD: u64 = 600 * PROBE_INTERVAL;
/// A round trip a member does not report.
const UNKNOWN: u64 = u64::MAX;

/// Round trips in ms: us-east, us-west, eu-west, ap-ne, sa-east (`wan_leaders`).
const WAN: [[u64; 5]; 5] = [
    [0, 70, 80, 180, 120],
    [70, 0, 140, 110, 170],
    [80, 140, 0, 230, 190],
    [180, 110, 230, 0, 300],
    [120, 170, 190, 300, 0],
];

/// Replica `me` of `members`, leading with `order`, placement on.
fn leader(me: u32, order: &[u32]) -> Replica {
    let members: Vec<NodeId> = order.iter().copied().map(NodeId).collect();
    let mut cfg = Config::new([3; 16], NodeId(me), members.clone(), [9; 32]);
    cfg.leader_order = members;
    cfg.placement = true;
    let mut r = Replica::new(cfg, [], Timestamp(0)).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
    r.handle(Timestamp(0), Input::Persisted(PersistId(0))).unwrap();
    r.poll_actions(&mut out);
    r
}

/// One probe round at `t`: every member reports `rtt` (ms, by id) fresh, and `writes`
/// application entries per origin were applied since the last round.
fn round(r: &mut Replica, t: u64, rtt: &[&[u64]], writes: &[(u32, u64)]) {
    let now = Timestamp(t);
    let me = r.me.0 as usize;
    for (a, row) in rtt.iter().enumerate() {
        let table: Vec<(NodeId, u64)> = row
            .iter()
            .enumerate()
            .filter(|(b, ms)| *b != a && **ms != UNKNOWN)
            .map(|(b, ms)| (NodeId(b as u32), ms * MS))
            .collect();
        if a == me {
            for (n, v) in table {
                r.placement.rtt.insert(n, (v, now));
            }
        } else if !table.is_empty() {
            // A member that reports nothing has sent no table.
            r.placement.tables.insert(NodeId(a as u32), (table, now));
        }
    }
    r.placement.writes = writes.iter().map(|&(o, n)| (NodeId(o), n)).collect();
    r.placement.last_write = Some(now);
    r.placement.next_probe = Some(now);
    r.handle_timeout(now).unwrap();
    let mut out = Vec::new();
    r.poll_actions(&mut out);
}

/// Rounds once a second from `from` for `secs` seconds; the time after the last one.
fn rounds(r: &mut Replica, from: u64, secs: u64, rtt: &[&[u64]], writes: &[(u32, u64)]) -> u64 {
    let mut t = from;
    for _ in 0..secs {
        round(r, t, rtt, writes);
        t += PROBE_INTERVAL;
    }
    t
}

/// The order placement submitted, if any.
fn moved(r: &Replica) -> Option<Vec<u32>> {
    let id = r.placement.policy.moving()?;
    let item = r
        .entries
        .pending()
        .iter()
        .find(|i| i.entry.id == id)
        .map(|i| i.entry.payload.clone())
        .or_else(|| {
            r.entries
                .batches()
                .values()
                .flat_map(|b| batch::changes(&b.value))
                .find(|(k, _)| k.2 == id)
                .map(|(_, p)| p)
        })
        .or_else(|| r.entries.forwarded().get(&id).map(|(i, _)| i.entry.payload.clone()))?;
    let (_, order) = crate::schedule::decode_change(&item)?;
    Some(order.into_iter().map(|n| n.0).collect())
}

fn wan() -> Vec<&'static [u64]> {
    WAN.iter().map(|r| &r[..]).collect()
}

fn three(x: u64) -> [[u64; 3]; 3] {
    [[0, 100, 100], [100, 0, x], [100, x, 0]]
}

fn rows(m: &[[u64; 3]; 3]) -> Vec<&[u64]> {
    m.iter().map(|r| &r[..]).collect()
}

/// Three members 2 ms apart, every one writing.
const LAN: [[u64; 3]; 3] = [[0, 2, 2], [2, 0, 2], [2, 2, 0]];
const ALL: [(u32, u64); 3] = [(0, 10), (1, 10), (2, 10)];

/// `rounds` samples of the writers' tail around `ms` (±1 ms) under `member`, over the
/// sustained period up to `at`.
fn led(r: &mut Replica, member: u32, ms: u64, rounds: u64, at: u64) {
    let mut l = Led::default();
    l.add(ms * 1_000, Timestamp(at.saturating_sub(SUSTAIN)));
    for k in 0..rounds {
        l.add(ms * 1_000 + if k % 2 == 0 { 1_000 } else { 0 }, Timestamp(at));
    }
    l.add(ms * 1_000 - 1_000, Timestamp(at));
    r.placement.policy.insert_led(NodeId(member), l);
}

/// The member a trial or move put first, if any.
fn first(r: &Replica) -> Option<u32> {
    moved(r).map(|o| o[0])
}

/// Replica 0 leading, its tenure measured at `ms`; one probe round after the sustained
/// period.
fn explored(ms: u64, others: &[(u32, u64, u64)]) -> Replica {
    let mut r = leader(0, &[0, 1, 2]);
    led(&mut r, 0, ms, 20, SUSTAIN);
    for &(m, ms, at) in others {
        led(&mut r, m, ms, 20, at);
    }
    r
}

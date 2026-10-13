//! Hot paths of the replica through its public API (codec, recorder and proposer included).

use std::collections::VecDeque;
use std::hint::black_box;

use bytes::Bytes;
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use quepaxa::{Action, Config, Input, NodeId, Replica, Timestamp};

fn replica(id: u32, n: u32) -> Replica {
    let cfg = Config::new([5; 16], NodeId(id), (0..n).map(NodeId).collect(), [id as u8 + 1; 32]);
    Replica::new(cfg, [], Timestamp(0)).unwrap()
}

/// Delivers every action between `rs` instantly (persists complete at once) until quiet;
/// returns how many entries replica 0 committed.
fn settle(rs: &mut [Replica], queue: &mut VecDeque<(usize, Input)>) -> usize {
    let mut committed = 0;
    let mut out = Vec::new();
    loop {
        for (i, r) in rs.iter_mut().enumerate() {
            r.poll_actions(&mut out);
            for a in out.drain(..) {
                match a {
                    Action::Send(o) => queue.push_back((o.to.0 as usize, Input::Message(o.bytes))),
                    Action::Persist { id, then, .. } => {
                        queue.extend(then.into_iter().map(|o| (o.to.0 as usize, Input::Message(o.bytes))));
                        queue.push_back((i, Input::Persisted(id)));
                    }
                    Action::Commit { entries, .. } if i == 0 => committed += entries.len(),
                    _ => {}
                }
            }
        }
        let Some((to, input)) = queue.pop_front() else { return committed };
        rs[to].handle(Timestamp(1), input).unwrap();
    }
}

fn bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("replica");
    g.throughput(Throughput::Elements(1));

    let mut single = [replica(0, 1)];
    let mut q = VecDeque::new();
    settle(&mut single, &mut q);
    g.bench_function("single_member_propose_to_commit", |b| {
        b.iter(|| {
            single[0].propose(Timestamp(1), Bytes::from_static(b"PUT abcdefgh 1")).unwrap();
            black_box(settle(&mut single, &mut q))
        })
    });

    let mut three = [replica(0, 3), replica(1, 3), replica(2, 3)];
    settle(&mut three, &mut q);
    g.bench_function("three_members_leader_propose_to_commit", |b| {
        b.iter(|| {
            three[0].propose(Timestamp(1), Bytes::from_static(b"PUT abcdefgh 1")).unwrap();
            black_box(settle(&mut three, &mut q))
        })
    });

    let mut lone = replica(0, 3);
    let mut out = Vec::new();
    lone.poll_actions(&mut out);
    let junk = Bytes::from(vec![0x51; 128]);
    g.bench_function("reject_malformed_message", |b| {
        b.iter(|| {
            lone.handle(Timestamp(1), Input::Message(junk.clone())).unwrap();
            lone.poll_actions(&mut out);
        })
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);

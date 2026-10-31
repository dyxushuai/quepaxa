# quepaxa

[![crates.io](https://img.shields.io/crates/v/quepaxa.svg)](https://crates.io/crates/quepaxa)
[![docs.rs](https://img.shields.io/docsrs/quepaxa)](https://docs.rs/quepaxa)
[![CI](https://github.com/dyxushuai/quepaxa/actions/workflows/ci.yml/badge.svg)](https://github.com/dyxushuai/quepaxa/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/crates/msrv/quepaxa)](#platforms)
[![no_std](https://img.shields.io/badge/no__std-alloc-blue)](#platforms)
[![license](https://img.shields.io/crates/l/quepaxa.svg)](#license)

An open-source Rust implementation of [QuePaxa](https://bford.info/pub/os/quepaxa/quepaxa.pdf)
(SOSP'23), a consensus algorithm for state machine replication whose liveness never depends
on timeouts. Any replica can drive any slot; a leader is only a fast path (one round trip),
and hedged, randomized proposers keep the replicated log moving when the leader is slow,
partitioned or gone. QuePaxa is the algorithm behind Cloudflare Research's
[Meerkat](https://blog.cloudflare.com/meerkat-introduction/) global consensus service.

- **No timeout tuning for availability.** A slow or dead leader costs latency, never a
  stall; there are no elections.
- **Sans-IO and `no_std`.** The core never touches the network, a disk or a clock: you
  feed it events and carry out the actions it returns, on any runtime (Tokio, threads,
  an event loop, a simulator).
- **Embeddable anywhere.** A stable C ABI with prebuilt libraries for Linux, macOS,
  Windows, Android and iOS.
- **Verified continuously.** A machine-checked (TLAPS) proof that the protocol never
  decides two values, deterministic simulation, model checking, fuzzing, Miri and mutation
  testing, all in CI ([details](#guarantees-and-verification)).

## Install

```sh
cargo add quepaxa bytes
```

On `no_std` targets, use `cargo add bytes --no-default-features` for `bytes`.

From C, C++, Go, Swift or another FFI language: download the prebuilt library for your
platform from the [releases](https://github.com/dyxushuai/quepaxa/releases) and read
[docs.rs/quepaxa-ffi](https://docs.rs/quepaxa-ffi).

## Quick start

A single-member cluster commits on its own. With more members, messages go to peers
(`Action::Send`, and each `Action::Persist`'s `then` once its records are durable) and
their replies come back through `Replica::handle`; the loop is the same.

```rust
use bytes::Bytes;
use quepaxa::{Action, Config, Input, NodeId, Replica, Timestamp};

fn main() -> Result<(), quepaxa::Error> {
    // Cluster id, this replica's id, the members, and a fresh random seed per start.
    let cfg = Config::new([7; 16], NodeId(0), vec![NodeId(0)], [42; 32]);
    // No durable records yet: a brand-new replica.
    let mut replica = Replica::new(cfg, [], Timestamp(0))?;
    let mut disk = Vec::new();
    let mut actions = Vec::new();

    replica.poll_actions(&mut actions); // after every call, before the next one
    replica.propose(Timestamp(0), Bytes::from_static(b"set x 1"))?;
    replica.poll_actions(&mut actions);

    let mut committed = Vec::new();
    while !actions.is_empty() {
        for action in std::mem::take(&mut actions) {
            match action {
                // Make the records durable (append + fsync), then send `then`, then report.
                Action::Persist { id, records, then, .. } => {
                    disk.extend(records);
                    assert!(then.is_empty()); // no peers here
                    replica.handle(Timestamp(0), Input::Persisted(id))?;
                }
                Action::Send(_out) => { /* deliver out.bytes to out.to */ }
                Action::Commit { entries, .. } => {
                    // Apply to your state machine, in slot order.
                    committed.extend(entries.into_iter().map(|e| e.payload));
                }
                _ => {}
            }
        }
        replica.poll_actions(&mut actions);
    }
    assert_eq!(committed, [Bytes::from_static(b"set x 1")]);
    Ok(())
}
```

Next: three replicas exchanging messages in
[`examples/cluster.rs`](https://github.com/dyxushuai/quepaxa/blob/main/crates/quepaxa/examples/cluster.rs), and the
[guide](https://docs.rs/quepaxa/latest/quepaxa/guide/index.html) for everything a production embedding needs.

## QuePaxa compared to Raft and Multi-Paxos

All three replicate a log across `n` replicas and stay available while a majority is up
(`2f + 1` replicas tolerate `f` crashes). In Raft and Multi-Paxos, progress depends on one
leader: when it crashes, the cluster waits for an election timeout; when it is slow but
still answers heartbeats, it stays leader and every write pays for it. Timeouts short
enough to recover quickly cause spurious elections on a jittery wide-area network, so they
are a tuning problem with no right answer.

QuePaxa keeps the leader's one-round-trip fast path in the common case, but never waits
for it: if a slot is not decided in time, other replicas hedge and propose too, and the
protocol still decides exactly one value. A timeout set too short costs extra messages,
not availability; a slow leader costs latency, not a stall. The price is a more involved
protocol (without the fast path, a round takes up to four phases, and a decision usually
three) and slightly more state per slot.

## Documentation

The API reference on [docs.rs/quepaxa](https://docs.rs/quepaxa) comes with a guide (also
readable in [`crates/quepaxa/docs`](https://github.com/dyxushuai/quepaxa/tree/main/crates/quepaxa/docs)):

| Chapter | What it covers |
|---|---|
| [Driving the replica](https://docs.rs/quepaxa/latest/quepaxa/guide/driving/index.html) | The event loop, every action, persist-before-send, timers, threads |
| [Durability and restart](https://docs.rs/quepaxa/latest/quepaxa/guide/durability/index.html) | What storage must promise, recovery, what breaks safety |
| [Snapshots](https://docs.rs/quepaxa/latest/quepaxa/guide/snapshots/index.html) | Compacting the log, installing a peer's snapshot |
| [Linearizable reads](https://docs.rs/quepaxa/latest/quepaxa/guide/reads/index.html) | Quorum reads on any replica, without a log slot |
| [Membership and leaders](https://docs.rs/quepaxa/latest/quepaxa/guide/membership/index.html) | Adding and removing members, choosing the leader order |
| [Tuning](https://docs.rs/quepaxa/latest/quepaxa/guide/tuning/index.html) | Every `Config` field, WAN settings, metrics |
| [How the protocol works](https://docs.rs/quepaxa/latest/quepaxa/guide/protocol/index.html) | QuePaxa in a few pages, and a glossary |

## Examples

| Example | Shows |
|---|---|
| `cargo run -p quepaxa --example cluster` | Three replicas, writes and a linearizable read |
| `cargo run -p quepaxa --example restart` | Crash recovery, compaction, catching up from a snapshot |
| `cargo run -p quepaxa --example membership` | Replacing a member and the leader |
| [`examples/kv-tokio`](https://github.com/dyxushuai/quepaxa/tree/main/examples/kv-tokio) | A networked key-value store: TCP, fsync'd storage, snapshots, membership changes, a load generator |
| [`crates/quepaxa-ffi/examples/smoke.c`](https://github.com/dyxushuai/quepaxa/blob/main/crates/quepaxa-ffi/examples/smoke.c) | The same loop in C |

The first three share a small in-memory driver,
[`crates/quepaxa/examples/common`](https://github.com/dyxushuai/quepaxa/tree/main/crates/quepaxa/examples/common), written to be read.

## Guarantees and verification

- **Agreement and validity:** every replica applies the same entries in the same order,
  only entries that were proposed, each at most once.
- **Crash recovery:** replies never leave before the state they report is durable
  ([ADR 0002](https://github.com/dyxushuai/quepaxa/blob/main/docs/adr/0002-structural-persist-before-send.md)); a restarted replica
  never contradicts what it said before.
- **Linearizable reads** on any replica: one round to a majority, no log slot and no write.
- **Membership changes through the log** (`Replica::propose_reconfiguration`), effective
  64 slots after they are decided, and a configurable leader order (`Config::leader_order`)
  to put the leader where a majority is closest
  ([design](https://github.com/dyxushuai/quepaxa/blob/main/docs/design/reconfiguration.md)).

Checked by unit and property tests; a deterministic simulator (loss, duplication,
reordering, partitions, crash-restart with lost and torn writes, snapshots, membership
changes) with agreement and linearizability oracles; delay-bounded model checking of the
real replica; an independent TLA+ model, with a TLAPS proof that it never decides two
values for any number of replicas and rounds, and trace validation of the Rust code against
it; fuzzing; Miri; mutation testing with a ratchet;
and seeded-bug canaries that must keep failing. See [VERIFICATION.md](https://github.com/dyxushuai/quepaxa/blob/main/VERIFICATION.md).

## Performance

`examples/kv-tokio`, three replicas on one VM over loopback, every write `fsync`ed to a
virtual disk before it is acknowledged, half writes and half linearizable reads: 250,000
operations/s with p50 7.2 ms and p99 14 ms; at 5,000 operations/s, p50 5.9 ms. Latency is a few `fsync`s and barely depends on load. Setup, in-memory numbers and
simulated WAN placements: [docs/benchmarks.md](https://github.com/dyxushuai/quepaxa/blob/main/docs/benchmarks.md).
Against etcd v3.7.2 through etcd's own API and tools, including leader crashes,
partitions, slow disks and distant leaders: [docs/benchmarks.md#against-etcd](https://github.com/dyxushuai/quepaxa/blob/main/docs/benchmarks.md#against-etcd).

## Platforms

Tested natively on Linux, macOS and Windows (x86_64 and aarch64); the C ABI is tested on
Android (emulator) and iOS (simulator); the core builds for `wasm32-unknown-unknown` and
`thumbv7em-none-eabihf` (`no_std` with `alloc`). MSRV: Rust 1.89.

## Status

Pre-1.0. The wire format, durable format and C ABI may change between minor versions until
1.0; the [changelog](https://github.com/dyxushuai/quepaxa/blob/main/CHANGELOG.md) has upgrade notes. Not supported: Byzantine faults,
witnesses (vote-only members), automatic latency-based leader tuning. Design record:
[spec](https://github.com/dyxushuai/quepaxa/blob/main/.scratch/quepaxa-v1/spec.md), [decisions](https://github.com/dyxushuai/quepaxa/blob/main/.scratch/quepaxa/map.md),
[ADRs](https://github.com/dyxushuai/quepaxa/tree/main/docs/adr).

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/dyxushuai/quepaxa/blob/main/LICENSE-APACHE) or
[MIT license](https://github.com/dyxushuai/quepaxa/blob/main/LICENSE-MIT) at your option.

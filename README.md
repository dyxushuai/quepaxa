# quepaxa

[![crates.io](https://img.shields.io/crates/v/quepaxa.svg)](https://crates.io/crates/quepaxa)
[![docs.rs](https://img.shields.io/docsrs/quepaxa)](https://docs.rs/quepaxa)
[![CI](https://github.com/dyxushuai/quepaxa/actions/workflows/ci.yml/badge.svg)](https://github.com/dyxushuai/quepaxa/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/quepaxa.svg)](#license)

An open-source Rust implementation of [QuePaxa](https://bford.info/pub/os/quepaxa/quepaxa.pdf)
(SOSP'23), a consensus algorithm for state machine replication whose liveness never depends
on timeouts. Any replica can drive any slot; a leader is only a fast path (one round trip),
and hedged, randomized proposers keep the replicated log moving when the leader is slow,
partitioned or gone. QuePaxa is the algorithm behind Cloudflare Research's
[Meerkat](https://blog.cloudflare.com/meerkat-introduction/) global consensus service.

- **`quepaxa`** — sans-IO, `no_std + alloc` core exposing a replicated log of opaque
  batches. It never touches the network, disk or clock: you feed it events and execute the
  actions it returns, which makes it deterministic, simulation-tested and embeddable.
- **`quepaxa-ffi`** — stable C ABI (`cdylib` / `staticlib`, header in
  [`crates/quepaxa-ffi/include/quepaxa.h`](https://github.com/dyxushuai/quepaxa/blob/main/crates/quepaxa-ffi/include/quepaxa.h)).
- **`examples/kv-tokio`** — reference runtime: a replicated key-value store over TCP with
  fsync'd storage, snapshots and a load generator.

## QuePaxa compared to Raft and Multi-Paxos

All three replicate a log across `2f + 1` replicas and tolerate `f` crashes. In Raft and
Multi-Paxos, progress depends on one leader: when it crashes, the cluster waits for an
election timeout; when it is merely slow or badly placed, it is not replaced at all, and
every write pays for it. Timeouts short enough to recover quickly cause spurious elections
on a jittery wide-area network, so they are a tuning problem with no right answer.

QuePaxa keeps the leader's one-round-trip fast path in the common case, but never waits
for it: if a slot is not decided in time, other replicas hedge and propose too, and the
protocol still decides exactly one value. A timeout set too short costs extra messages,
not availability; a slow leader costs latency, not a stall. The price is a more involved
protocol (four phases per round when the fast path fails) and slightly more state per slot.

## Using the core

```rust
use bytes::Bytes;
use quepaxa::{Action, Config, Input, NodeId, Replica, Timestamp};

let cfg = Config::new(cluster_id, NodeId(0), members, fresh_csprng_seed);
let mut replica = Replica::new(cfg, durable_records, Timestamp(now))?;
let mut actions = Vec::new();
replica.poll_actions(&mut actions); // after every call, before the next one
// … execute the startup actions (the new incarnation's Persist), then:
replica.propose(Timestamp(now), Bytes::from_static(b"set x 1"))?;
replica.poll_actions(&mut actions);
for action in actions {
    match action {
        // Append durably after earlier persists (or replace the log if `replace`),
        // then send `then`, then report `Input::Persisted(id)`.
        Action::Persist { id, records, then, replace } => { /* … */ }
        Action::Send(out) => { /* deliver out.bytes to out.to */ }
        Action::Commit { slot, entries } => { /* apply in slot order */ }
        _ => {}
    }
}
```

Documentation: [docs.rs/quepaxa](https://docs.rs/quepaxa), with a guide in chapters — [driving the
replica](https://docs.rs/quepaxa/latest/quepaxa/guide/driving/index.html), [durability and
restart](https://docs.rs/quepaxa/latest/quepaxa/guide/durability/index.html),
[snapshots](https://docs.rs/quepaxa/latest/quepaxa/guide/snapshots/index.html), [linearizable
reads](https://docs.rs/quepaxa/latest/quepaxa/guide/reads/index.html), [membership and
leaders](https://docs.rs/quepaxa/latest/quepaxa/guide/membership/index.html),
[tuning](https://docs.rs/quepaxa/latest/quepaxa/guide/tuning/index.html) and [how the protocol
works](https://docs.rs/quepaxa/latest/quepaxa/guide/protocol/index.html) (also readable in
[`crates/quepaxa/docs`](https://github.com/dyxushuai/quepaxa/tree/main/crates/quepaxa/docs)).

Runnable examples: `cargo run -p quepaxa --example cluster` (three replicas, writes and a
linearizable read), `restart` (crash recovery, compaction, catching up from a snapshot) and
`membership` (replacing a member and the leader), all on the in-memory driver in
[`crates/quepaxa/examples/common`](https://github.com/dyxushuai/quepaxa/tree/main/crates/quepaxa/examples/common).
[`examples/kv-tokio`](https://github.com/dyxushuai/quepaxa/tree/main/examples/kv-tokio) is a complete networked, fsync'd key-value store. The C ABI
follows the same model: [docs.rs/quepaxa-ffi](https://docs.rs/quepaxa-ffi) and
[`crates/quepaxa-ffi/examples/smoke.c`](https://github.com/dyxushuai/quepaxa/blob/main/crates/quepaxa-ffi/examples/smoke.c).

## Guarantees and testing

- Safety under crash-recovery: replies never leave before the state they report is durable
  ([ADR 0002](https://github.com/dyxushuai/quepaxa/blob/main/docs/adr/0002-structural-persist-before-send.md)); leaders never reuse their
  fast-path priority after a restart.
- Linearizable reads through read barriers; commit-time dedup applies each entry once.
- Membership changes through the log (`Replica::propose_reconfiguration`), effective 64
  slots after they are decided, one at a time; a configurable leader order
  (`Config::leader_order`) to put the leader where a majority is closest
  ([design](https://github.com/dyxushuai/quepaxa/blob/main/docs/design/reconfiguration.md)).
- Verified by unit and property tests, a deterministic simulator (loss, duplication,
  reordering, partitions, crash-restart with lost and torn writes, snapshots) with
  agreement and linearizability oracles, delay-bounded model checking of the real replica,
  fuzzing, Miri, mutation testing with a ratchet, and seeded-bug canaries that must keep
  failing. See [VERIFICATION.md](https://github.com/dyxushuai/quepaxa/blob/main/VERIFICATION.md).

## Platforms

Tested natively on Linux, macOS and Windows (x86_64 and aarch64); the C ABI is tested on
Android (emulator) and iOS (simulator); the core builds for `wasm32-unknown-unknown` and
`thumbv7em-none-eabihf`. MSRV: Rust 1.89.

## Status

Pre-1.0. The wire format, durable format and C ABI may change between minor versions until
1.0 (see the [changelog](https://github.com/dyxushuai/quepaxa/blob/main/CHANGELOG.md) for upgrade notes). Not supported: Byzantine
faults, witnesses (vote-only members), automatic latency-based leader tuning. Performance
measurements, on disk and in memory: [docs/benchmarks.md](https://github.com/dyxushuai/quepaxa/blob/main/docs/benchmarks.md). Design record:
[spec](https://github.com/dyxushuai/quepaxa/blob/main/.scratch/quepaxa-v1/spec.md), [decisions](https://github.com/dyxushuai/quepaxa/blob/main/.scratch/quepaxa/map.md),
[ADRs](https://github.com/dyxushuai/quepaxa/tree/main/docs/adr).

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/dyxushuai/quepaxa/blob/main/LICENSE-APACHE) or
[MIT license](https://github.com/dyxushuai/quepaxa/blob/main/LICENSE-MIT) at your option.

# quepaxa

An open-source Rust implementation of [QuePaxa](https://bford.info/pub/os/quepaxa/quepaxa.pdf)
(SOSP'23): crash-fault-tolerant consensus whose liveness never depends on timeouts. Any
replica can drive any slot; a leader is only a fast path (one round trip), and hedged,
randomized proposers keep the log moving when the leader is slow, partitioned or gone.

- **`quepaxa`** — sans-IO, `no_std + alloc` core exposing a replicated log of opaque
  batches. It never touches the network, disk or clock: you feed it events and execute the
  actions it returns, which makes it deterministic, simulation-tested and embeddable.
- **`quepaxa-ffi`** — stable C ABI (`cdylib` / `staticlib`, header in
  [`crates/quepaxa-ffi/include/quepaxa.h`](https://github.com/dyxushuai/quepaxa/blob/main/crates/quepaxa-ffi/include/quepaxa.h)).
- **`examples/kv-tokio`** — reference runtime: a replicated key-value store over TCP with
  fsync'd storage, snapshots and a load generator.

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

The crate docs and [`examples/kv-tokio`](https://github.com/dyxushuai/quepaxa/tree/main/examples/kv-tokio) show the complete driving loop,
restart, compaction and snapshot installation. The C ABI follows the same model; see
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

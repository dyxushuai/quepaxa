# quepaxa

An open-source Rust implementation of [QuePaxa](https://bford.info/pub/os/quepaxa/quepaxa.pdf)
(SOSP'23): crash-fault-tolerant consensus whose liveness never depends on timeouts. Any
replica can drive any slot; a leader is only a fast path (one round trip), and hedged,
randomized proposers keep the log moving when the leader is slow, partitioned or gone.

- **`quepaxa`** — sans-IO, `no_std + alloc` core exposing a replicated log of opaque
  batches. It never touches the network, disk or clock: you feed it events and execute the
  actions it returns, which makes it deterministic, simulation-tested and embeddable.
- **`quepaxa-ffi`** — stable C ABI (`cdylib` / `staticlib`, header in
  [`crates/quepaxa-ffi/include/quepaxa.h`](crates/quepaxa-ffi/include/quepaxa.h)).
- **`examples/kv-tokio`** — reference runtime: a replicated key-value store over TCP with
  fsync'd storage, snapshots and a load generator.

## Using the core

```rust
use bytes::Bytes;
use quepaxa::{Action, Config, Input, NodeId, Replica, Timestamp};

let cfg = Config::new(cluster_id, NodeId(0), members, fresh_csprng_seed);
let mut replica = Replica::new(cfg, durable_records, Timestamp(now))?;
replica.propose(Timestamp(now), Bytes::from_static(b"set x 1"))?;
let mut actions = Vec::new();
replica.poll_actions(&mut actions); // after every call
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

The crate docs and [`examples/kv-tokio`](examples/kv-tokio) show the complete driving loop,
restart, compaction and snapshot installation. The C ABI follows the same model; see
[`crates/quepaxa-ffi/examples/smoke.c`](crates/quepaxa-ffi/examples/smoke.c).

## Guarantees and testing

- Safety under crash-recovery: replies never leave before the state they report is durable
  ([ADR 0002](docs/adr/0002-structural-persist-before-send.md)); leaders never reuse their
  fast-path priority after a restart.
- Linearizable reads through read barriers; commit-time dedup applies each entry once.
- Verified by unit and property tests, a deterministic simulator (loss, duplication,
  reordering, partitions, crash-restart with lost and torn writes, snapshots) with
  agreement and linearizability oracles, delay-bounded model checking of the real replica,
  fuzzing, Miri, and mutation testing. See the [spec](.scratch/quepaxa-v1/spec.md) §9.

## Platforms

Tested natively on Linux, macOS and Windows (x86_64 and aarch64); the C ABI is tested on
Android (emulator) and iOS (simulator); the core builds for `wasm32-unknown-unknown` and
`thumbv7em-none-eabihf`. MSRV: Rust 1.89.

## Status

Pre-1.0. The wire format, durable format and C ABI may change between minor versions until
1.0. Not in v1: membership reconfiguration, Byzantine faults, latency-tuned leader order
(#28). Performance measurements: [docs/benchmarks.md](docs/benchmarks.md). Design record:
[spec](.scratch/quepaxa-v1/spec.md), [decisions](.scratch/quepaxa/map.md),
[ADRs](docs/adr/).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

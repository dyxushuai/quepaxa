# quepaxa

An open-source Rust implementation of [QuePaxa](https://bford.info/pub/os/quepaxa/quepaxa.pdf)
(SOSP'23), a crash-fault-tolerant consensus protocol whose liveness never depends on timeouts.

- `quepaxa`: sans-IO, `no_std + alloc` core exposing a replicated log of opaque batches.
- `quepaxa-ffi`: stable C ABI (`cdylib` / `staticlib`) for other languages.
- `quepaxa-sim`: deterministic simulator and invariant oracles (not published).

Status: pre-release, under construction. The design lives in
[`.scratch/quepaxa-v1/spec.md`](.scratch/quepaxa-v1/spec.md); decisions and their rationale in
[`.scratch/quepaxa/map.md`](.scratch/quepaxa/map.md) and [`docs/adr/`](docs/adr/).

Out of scope for v1: Byzantine faults, membership reconfiguration, application layers (KV, leases).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.

# Changelog

All notable changes to this project are documented here. The project follows
[Semantic Versioning](https://semver.org/); before 1.0 any minor version may be breaking.

## 0.2.0 — 2026-10-13

First real release (0.1.0 was an unrelated name placeholder and will be yanked).

- `quepaxa`: sans-IO `no_std` QuePaxa replica — leader fast path, randomized leaderless
  rounds, forwarding and adaptive hedging, deterministic leader rotation, batching and
  pipelining, structural persist-before-send durability with crash-restart recovery,
  bulk catch-up, read barriers, commit-time dedup, compaction with checkpoints and
  snapshot installation.
- `quepaxa-ffi`: C ABI 0.3 with a committed header.
- `examples/kv-tokio`: reference runtime and `kv-bench` load generator.

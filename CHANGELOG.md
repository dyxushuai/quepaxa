# Changelog

All notable changes to this project are documented here. The project follows
[Semantic Versioning](https://semver.org/); before 1.0 any minor version may be breaking.

## Unreleased

- `quepaxa` **durable format**: recorder registers are written as a new record kind that
  stores a value repeated across `first`/`agg`/`prev_agg` once (about 30% fewer bytes
  persisted on the fast path). Records written by 0.2 are still read; 0.2 cannot read
  records written by this version, so downgrading needs a fresh data directory.

- `quepaxa`: CRC-32C for durable records uses slicing-by-8 (about 3-4x faster record
  encoding); `Committed` documents that a snapshot install reports the snapshot's slot;
  `install_snapshot` documents that the application snapshot must be durable before the
  checkpoint persist runs.
- `examples/kv-tokio`: batched peer and client I/O and background snapshot writes
  (saturation near 200k ops/s instead of 100k on the reference VM); a `STATS` command, which
  `kv-bench` prints after each run.

## 0.2.0 — 2026-10-13

First real release (0.1.0 was an unrelated name placeholder and will be yanked).

- `quepaxa`: sans-IO `no_std` QuePaxa replica — leader fast path, randomized leaderless
  rounds, forwarding and adaptive hedging, deterministic leader rotation, batching and
  pipelining, structural persist-before-send durability with crash-restart recovery,
  bulk catch-up, read barriers, commit-time dedup, compaction with checkpoints and
  snapshot installation.
- `quepaxa-ffi`: C ABI 0.3 with a committed header.
- `examples/kv-tokio`: reference runtime and `kv-bench` load generator.

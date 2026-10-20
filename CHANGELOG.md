# Changelog

All notable changes to this project are documented here. The project follows
[Semantic Versioning](https://semver.org/); before 1.0 any minor version may be breaking.

## Unreleased

- `quepaxa`: `Action::Retired` is reported only after a read barrier of the replica's own
  commits and shows no later change added it back (an internal barrier, never reported as
  `ReadReady`). Before, a replica added back by a change decided after the new members
  answered its progress probes could still report `Retired`; runtimes no longer need their
  own barrier for this. A change decided after the barrier remains the operator's to
  handle.
- `quepaxa`: requests (to record, to catch up, forwarded entries) are served for any
  replica of the cluster, not only those of a known epoch, so a joiner removed again
  can still retire after a further change pruned every epoch that listed it. Replies and
  decisions from unknown replicas are still rejected.

- `examples/kv-tokio`: membership changes. `--peers` lists replicas that may join later,
  the client command `RECONFIGURE <ids> [LEADERS <ids>]` proposes a change (only naming
  replicas with an address), and a removed replica exits with status 0 once it reports
  `Retired`; a process-level test replaces a member.

## 0.3.2 — 2026-10-16

- `quepaxa`: `Action::Retired { epoch }` tells a replica removed by a membership change
  when it may shut down: once a majority of the new members durably know every slot of
  the old epoch. Shutting down as soon as it had applied the old epoch, as the docs said,
  stalled the cluster for good when a majority of the old members left before the new
  ones learned those slots (found by the nightly simulator, seed 1534). Keep removed
  replicas running until they report `Retired`.
- `quepaxa`: catch-up replies are sent once the decisions they report are durable, and
  even when empty (no wire change).
- `quepaxa-ffi` (ABI 0.6): `QPX_ACTION_RETIRED` (epoch in `id`).
- Upgrading: `Retired` relies on catch-up replies from the new members, which older
  versions send before their decisions are durable. Upgrade every replica before you rely
  on it to remove one.
- crates.io and docs.rs: clearer descriptions, keywords and categories; the README has an
  install section, a runnable quick start (tested as a doctest), a comparison with Raft
  and Multi-Paxos, and tables of guide chapters and examples.
- Verification: the nightly simulation runs its 100k seeds per scenario across 8 machines
  (it never finished within its time limit before), mutation testing caps each test
  process's memory, and a new canary plants the old retirement rule.

## 0.3.1 — 2026-10-16

- Documentation: a `quepaxa::guide` in seven chapters (driving the replica, durability and
  restart, snapshots, linearizable reads, membership and leaders, tuning, how the protocol
  works), every public method documented with its errors, runnable examples
  (`cargo run -p quepaxa --example cluster | restart | membership`), and C-specific docs
  for `quepaxa-ffi`. CI builds the docs with warnings denied and runs the examples.
- `quepaxa`: `Config::max_batch_bytes` must be at least 64 (smaller values are
  `InvalidConfig`, `QPX_ERR_INVALID_CONFIG` in C), so a read barrier always fits a batch.
  Smaller values could not carry any entry before.
- `quepaxa`: `Commit` is emitted as soon as a slot is decided, without waiting for its
  local `Decided` record (a decision is durable at a majority of recorders); on disk this
  takes one fsync off every replica's commit path.
- `examples/kv-tokio`: persists are written on their own thread (group commit while the
  event loop keeps running); `run-local.sh` keeps data on disk (it used tmpfs, which made
  the published end-to-end latencies fsync-free). On the reference VM's disk, p50 went
  from 11.6 ms to 5.9 ms at 5k ops/s.

## 0.3.0 — 2026-10-14

### Upgrading from 0.2

- Upgrade replicas one at a time; data directories written by 0.2 are read as they are.
  While any replica still runs 0.2: do **not** propose membership changes (0.2 cannot
  decode them and would keep the old majorities), and expect a lagging 0.2 replica to be
  unable to install a snapshot from a 0.3 peer (`core_state` version 4).
- There is no downgrade: 0.2 cannot read the records and `core_state` 0.3 writes. Never
  restart a replica identity with an empty data directory (it would reuse its incarnation
  and leader reservation); restore a pre-upgrade backup instead, or add a new replica.
- An existing cluster keeps its by-id leader order: a custom `leader_order` applies only
  to a new cluster (or arrives with a membership change), because a replica refuses state
  built under another order.
- C ABI 0.5: `qpx_config` grew at the end (`leader_order`); callers built against 0.3 or
  0.4 keep working with the defaults. A new action kind (`QPX_ACTION_RECONFIGURED`)
  appears only if the cluster changes membership.
- `Config` no longer requires `id` to be a member (a joining replica), but rejects empty
  `members` and more than 1024.

### Added

- **Membership reconfiguration** — `Replica::propose_reconfiguration` changes the members
  (and preferred leaders) of a running cluster; a change decided in slot `s` takes effect
  at `s + 64` (`Action::Reconfigured`), one at a time. Joining replicas run with the
  original `Config` and catch up during the window. C ABI 0.5:
  `qpx_propose_reconfiguration`, `QPX_ACTION_RECONFIGURED` with `qpx_action_member`,
  `QPX_ERR_RECONFIGURATION_PENDING`. Design: `docs/design/reconfiguration.md`.
- `Config::leader_order` (C ABI 0.4: `qpx_config.leader_order`): the preferred leaders,
  first to last; on a simulated WAN the leader's placement alone changes client latency
  about 2x. A custom order is mixed into the wire cluster identity and recorded in
  `core_state`, so differently configured members ignore each other and a replica refuses
  state built under another order.
- `examples/kv-tokio`: `--leader-order`; a `STATS` command, printed by `kv-bench`.

### Performance

- Recorder records store a value repeated across `first`/`agg`/`prev_agg` once (about 30%
  fewer bytes persisted on the fast path); CRC-32C uses slicing-by-8.
- `examples/kv-tokio`: batched peer and client I/O, snapshots shared by shard and written
  off the event loop, mimalloc. Saturation near 200k ops/s instead of 100k on the
  reference VM; 100k/s p90 0.9 ms instead of 31 ms (`docs/benchmarks.md`).

### Fixed

- A replica far behind asks for a newer snapshot at once if peers compacted past the one
  it just installed (it waited `hedge_max`).
- kv-tokio writes nothing more after a failed checkpoint, and never serves or keeps an
  older snapshot than one it already saved.
- Docs: `Committed` after a snapshot install reports the snapshot's slot;
  `install_snapshot` requires the application snapshot to be durable before the checkpoint
  persist runs; `retransmit_interval` and `hedge_max` guidance for WANs.

### Verification

- Seeded-bug canaries on every PR (12), the durability gate tested on its own, one
  client-history oracle for the simulator and the model checker, a mutation-testing
  ratchet, WAN placement and membership-change simulations (`VERIFICATION.md`).

## 0.2.0 — 2026-10-13

First real release (0.1.0 was an unrelated name placeholder and will be yanked).

- `quepaxa`: sans-IO `no_std` QuePaxa replica — leader fast path, randomized leaderless
  rounds, forwarding and adaptive hedging, deterministic leader rotation, batching and
  pipelining, structural persist-before-send durability with crash-restart recovery,
  bulk catch-up, read barriers, commit-time dedup, compaction with checkpoints and
  snapshot installation.
- `quepaxa-ffi`: C ABI 0.3 with a committed header.
- `examples/kv-tokio`: reference runtime and `kv-bench` load generator.

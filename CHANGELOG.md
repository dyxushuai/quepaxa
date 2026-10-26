# Changelog

All notable changes to this project are documented here. The project follows
[Semantic Versioning](https://semver.org/); before 1.0 any minor version may be breaking.

## Unreleased

- Verification: VERIFICATION.md maps each TLA+ action to the Rust code it stands for and
  what the model leaves out; `prove.sh` refuses to run if a module stating a theorem is
  not in its list.
- Verification: TLAPS proofs of the membership model's window rule (every replica that
  may drive a slot computes its true members) and of retirement (no decided slot is lost),
  for any number of replicas, slots, members and changes. Assumed: a replica a change
  adds back keeps running until it applied the change (the guide's rule), and no change
  names a replica that shut down (restarting a re-added replica is not covered).
- Guide (membership): during a change the cluster tolerates losing a minority of the old
  members and of the new ones, not of the new ones alone, until a majority of the new
  members durably hold the old slots.
- Verification: a TLAPS proof that the one-slot protocol model (`QuePaxa.tla`) never decides
  two values, for any number of recorders and proposers and every step bound
  (`verification/tla/prove.sh`, nightly). See VERIFICATION.md for what it covers.

## 0.4.0 — 2026-10-24

- `quepaxa` (breaking): only the current durable and snapshot formats are read. Recorder
  records of kind 2 and core states of versions 2 and 3 are rejected (`CorruptRecord`,
  `InvalidCoreState`) instead of converted; before 1.0 a replica whose storage predates a
  format change rejoins as a new replica (new id, empty storage).
- `quepaxa-ffi` (breaking, ABI 0.7): no configuration or metrics struct is part of the ABI
  any more. `qpx_config` is an opaque object: `qpx_config_new(cluster_id, id, members,
  n, seed, &cfg)`, `qpx_config_set(cfg, QPX_CONFIG_*, value)`,
  `qpx_config_set_leader_order`, `qpx_config_free`; `qpx_replica_new` copies it. Metrics
  are read one at a time with `qpx_metric(h, QPX_METRIC_*, &value)`. An unknown option or
  metric is an explicit `QPX_ERR_INVALID_ARGUMENT`, so new ones never change a layout.
  Removed: the `qpx_config`/`qpx_metrics` structs, `struct_size`, `qpx_config_init`,
  `qpx_get_metrics`.
- `examples/kv-tokio`: tokio 1.53.3 (RUSTSEC-2026-0386; `AsyncFd`, which kv-tokio does not use).

## 0.3.4 — 2026-10-23

- `quepaxa`: a decided membership change takes effect without traffic: while it is
  pending, the slots before it are taken over like stalled slots (the leader first, after
  a hedge delay; the others if it is down) and filled with empty batches. Before, in a quiet
  cluster the change never took effect, and a replica it removed probed forever without
  reporting `Retired`.
- `quepaxa`: `Retired` is decided once everything learned with the retirement barrier is
  applied, so a catch-up page or snapshot that also adds the replica back no longer
  produces `Retired` and the re-adding `Reconfigured` in the same call.
- `quepaxa`: a restarted single-member cluster has no catch-up timer (it had one forever,
  with nobody to ask).
- `quepaxa`: after a restart a replica keeps asking where the log is until a majority of
  the members answered (the members as of what it has learned, so a page that takes it
  into a new epoch needs that epoch's members); one answer from a peer as far behind as
  itself used to end it, and in a quiet cluster it then never caught up.
- `quepaxa`: a snapshot covering a local batch that was decided but not yet applied
  releases it (it stayed in memory forever).
- Guide (snapshots): restoring after a crash applies only the re-emitted slots after the
  snapshot's `through`; the example applied all of them, so a crash between storing a
  snapshot and its checkpoint becoming durable applied some operations twice.
- `examples/kv-tokio`: log frames and snapshots carry checksums. On restart only
  a torn tail is truncated; a damaged length field or record before it stops the restart
  instead of silently cutting acknowledged records. The data directory and its parents are
  synced on every start. The on-disk format changed: start from an empty data directory.
- `examples/kv-tokio`: bounded resources under misbehaving inputs. A client
  connection holds about 1024 unanswered requests and 1 MiB per line; a disk that falls
  more than 64 MiB behind stalls the event loop instead of buffering; a replica id listed
  twice in `--members`/`--peers` is rejected; peer reconnects back off up to 5 s; `kv-bench`
  no longer waits forever on a stalled server.

## 0.3.3 — 2026-10-20

- `quepaxa`: `Action::Retired` is reported only after a read barrier of the replica's own
  commits and shows no later change added it back (an internal barrier, never reported as
  `ReadReady`). Before, a replica added back by a change decided after the new members
  answered its progress probes could still report `Retired`; runtimes no longer need their
  own barrier for this. `Retired` now comes one committed slot later than in 0.3.2. A
  change decided after the barrier remains the operator's to handle.
- `quepaxa`: after a restart, a replica asks every known replica where the log is (sent
  with the incarnation's persist, and again on the retransmit timer until one answers).
  A replica that was down for its own removal, restarted into a quiet cluster, never
  learned of it and so never reported `Retired`.
- `quepaxa`: requests (to record, to catch up, forwarded entries) are served for any
  replica of the cluster, not only those of a known epoch, so a joiner removed again can
  still retire after a further change pruned every epoch that listed it. Replies and
  decisions from unknown replicas are still rejected.
- Guide: once any replica removed by a change reports `Retired`, every replica that
  change removed may shut down (unless added back), which covers one that is down for
  good or whose known peers were all replaced.
- `examples/kv-tokio`: membership changes. `--peers` lists replicas that may join later,
  the client command `RECONFIGURE <ids> [LEADERS <ids>]` proposes a change (only naming
  replicas with an address), and a removed replica exits with status 0 once it reports
  `Retired`; a process-level test replaces a member.
- Verification: Kani proves the recorder invariants for every register state; a TLA+
  model of membership, durability and retirement is checked exhaustively; logs of the
  real proposer and recorders are validated against the TLA+ spec.

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

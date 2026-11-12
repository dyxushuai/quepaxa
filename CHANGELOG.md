# Changelog

All notable changes to this project are documented here. The project follows
[Semantic Versioning](https://semver.org/); before 1.0 any minor version may be breaking.

## Unreleased

- `quepaxa`: `Metrics` gains `suspicions`, `placement_moves` and the `epoch` and
  `leader_position` gauges; `Replica::members` lists the members in force in leader order
  with each one's round trip, last reported commit index and last contact. C (ABI 0.10):
  matching `QPX_METRIC_*` constants and `QPX_CONFIG_PLACEMENT`,
  `QPX_CONFIG_PLACEMENT_SUSTAIN_NS`, `QPX_CONFIG_PLACEMENT_HOLD_NS`. kv-tokio:
  `--placement on`, and `STATS` ends with `LEADERS <ids>`.
- `quepaxa` (wire): leader placement. With `Config::placement` (off by default) on every
  member, members measure round trips to each other while writes flow (two new message
  kinds, `RttProbe` and `RttReply`; peers of older versions reject them) and count
  writes per origin, and the leader moves the lead to the member that gives writers the
  lowest commit latency once it keeps a 20% gain for `placement_sustain` (3 minutes)
  and no order change happened within `placement_hold` (10 minutes); the gain must also
  exceed 5 ms a write, so round trips within a datacenter never move it. Round trips
  cannot see a leader slowed by its CPU or disk, so members also report the writers' tail
  commit latency under each leader, and the members the round trips cannot tell apart
  are each led for a sustained period once (again after eight holds); the fastest then
  leads if the leader is slower by three standard errors and a tenth. This is the paper's
  §5.3 tuning restricted to members the round trips rank alike: the paper's version,
  exploring every member, was slower in every scenario measured. A membership change
  without a leader order puts the staying members in order of their cost.
- `quepaxa`: `Replica::propose_leader_order` changes only the leader order of the members
  in force; kv-tokio exposes it as `LEADERS <ids>`. In C,
  `qpx_propose_reconfiguration` with the current members and a new order does the same.
- `quepaxa` (breaking, wire and storage): a membership change carries the generation of
  the leader orders it was computed from (changes applied plus rotations counted), and
  every replica ignores one whose generation is out of date. Before, a change decided
  after another took effect could undo it. The ignored change's proposer gets
  `Dropped { reason: Superseded }` (C: `QPX_DROP_SUPERSEDED`, ABI 0.9) instead of
  `Committed`; one resolved by a snapshot gets `Unresolved` (`QPX_DROP_UNRESOLVED`).
  Snapshots no longer keep every change's id; older snapshots are rejected.

## 0.6.0 — 2026-11-06

Recovery from a degraded leader, measured against etcd v3.7.2 on three and five members and
across five emulated regions ([docs/benchmarks.md](docs/benchmarks.md#against-etcd)).

- `quepaxa` (breaking, wire): losing the leader across regions stalls the other regions
  about one commit latency instead of several hedge delays (`Config::silent_leader_takeover`,
  on by default). A follower whose leader says nothing at all for about a commit latency
  after a forward suspects it, hands everything to the next member of the leader order,
  which proposes it, and takes over the leader's unfinished slots as soon. In the
  five-region benchmark the stall falls from 3.2 s to 0.7-0.9 s and p99 after the loss
  from 7-8 s to about 1 s. A new message kind, `ForwardAck` (sent at once, before any
  write, at most once per eighth of `retransmit_interval` per follower), keeps a leader
  stalled on its disk from looking silent; peers of older versions reject it. A forward
  that arrives during the cooldown is acknowledged when it expires, even while
  persistence is stalled. Silence is judged per member, from a forward to that member,
  against how fast it answers.
- `quepaxa`: a follower no longer hedges around a leader that is slow but still deciding
  slots: an entry it forwarded is proposed here after four hedge delays (at most
  `hedge_max`) instead of one once the leader decided a slot itself since the forward, and
  hedges around a talking leader run one slot at a time. In the simulator, a shared disk
  that suddenly slows from 2 to 40 ms per write set off about 400 hedges and writes up to
  210-410 ms; now none, and at most 79-113 ms. In Docker's slow-disk case the median
  longest gap fell from 357 to 56 ms.
- `quepaxa`: a replica whose disk is stuck steps aside, if others hand it their entries
  (the leader or a deputy): once one of its writes has waited four hedge delays (at most
  `hedge_max`, at least four usual write times) while another replica decided a slot, it
  hands its pending entries to the next member of the leader order and stops
  acknowledging forwards, until its disk catches up. Not when no other replica decides
  either (one shared disk). With the leader's disk capped at 1 IOPS for 15 s, writes from
  its own clients reach p99 12-18 ms instead of 43-47 ms; etcd fails tens of thousands of
  requests in the same case. Counted in `Metrics::stepped_aside`.
- `quepaxa`: reads use a separate proposal-id range, keeping logged request ids contiguous
  so deduplication state no longer grows with every write after a read. `ProposalId` stays
  opaque and unique per incarnation; interleaved reads and writes are not ordered by id.
  Exhausting either range returns the new `Error::RequestIdExhausted` (restart from
  durable records). Wire and durable encodings are unchanged; existing dedup windows are
  kept until normal incarnation eviction.
- `quepaxa-ffi` (breaking): ABI 0.8 adds `QPX_ERR_REQUEST_ID_EXHAUSTED` and
  `QPX_METRIC_STEPPED_ASIDE`. Request ids remain `uint64_t`; read ids now use its high bit.
  Function signatures and output layouts are unchanged.
- `examples/kv-tokio`: client results are matched only to entries of the current
  `Replica::incarnation()`, so a delayed pre-restart request sharing an id, payload and slot
  with a new one can no longer answer it with the old result.
- `examples/kv-tokio`: a peer connection is redialed, with backoff capped at 1 s, when a
  connect takes over 2 s or the socket takes over 2 s to accept one 64 KiB chunk of a frame,
  so a large snapshot is cut off only on a link slower than about 32 KiB/s. A replica cut off by a
  partition rejoined about 11 s after the heal (TCP's own retransmission backoff); it now
  rejoins in about 0.6 s.
- `examples/kv-tokio`: `max_outstanding_proposals` is 65,536, so a brief CPU stall queues
  requests instead of failing them with `Busy`.
- `examples/kv-tokio` benchmarks: `bench/etcd/wan.sh` compares QuePaxa and etcd across
  five emulated regions; `run.sh` takes `NODES=5` and a `stuck-disk` scenario with clients
  on every member, and local artifacts no longer invalidate the comparison image.
- Verification: the simulator stalls or slows chosen replicas' disks
  (`tests/disk_faults.rs`) and loses a leader across regions (`tests/leader_loss.rs`); five
  new seeded-bug canaries guard silence detection, the busy-leader hedge wait and stepping
  aside.

## 0.5.0 — 2026-10-31

- `quepaxa` (breaking): linearizable reads no longer go through the log. `read_barrier` asks a
  majority of every member set from the decided prefix on for their high-water mark and
  is ready once the replica applied every slot below the largest one: one round trip, no
  consensus slot, no write. `read_barrier` and `ReadReady` keep their shape (the slot is the
  last one applied); reads still count against `max_outstanding_proposals` and are dropped
  on fence. Two message kinds join the wire format (`ReadQuery`, `ReadReply`); peers of
  older versions reject them. Modelled in `verification/tla/Reads.tla`, proved for fixed
  membership in `ReadsProof.tla` (ADR 0003).
- `quepaxa`: the hedge delay no longer feeds on itself. It learned from every own entry's
  commit latency, hedged ones included, whose latency contains the delay itself: once the
  leader slowed, each hedge raised the delay for the next, up to `hedge_max` in about a
  second (seen as a 1.1 s stall with a distant leader). It now learns only from entries
  that were not hedged, and is four times their smoothed latency instead of twice, so a
  healthy leader's ordinary tail no longer triggers hedges.
- `examples/kv-tokio`: serves etcd v3's KV gRPC API (`--etcd ADDR`: `Range`, `Put`,
  `DeleteRange`, single-level `Txn`) from the same replica and state as the line protocol,
  so etcd's clients and its `benchmark` tool drive it; anything outside that subset (past
  revisions, sorting, leases, nested or duplicate-key transactions, compaction) is refused
  with an explicit status. The store keeps etcd's revisions (store revision, per-key create
  and mod revision, version). Log entries and snapshots change format; existing data
  directories must be recreated. Adds tonic 0.14, prost 0.14 and protox 0.10 (64 crates
  in `Cargo.lock`, all checked by `cargo deny`), for the example only.
- `examples/kv-tokio`: `kv-bench --protocol etcd` drives etcd's API open loop and reports
  latency over time, the longest gap and degraded time after a fault; `bench/etcd/run.sh`
  compares a QuePaxa cluster with etcd v3.7.2 in Docker under steady load and four leader
  faults. Results: [docs/benchmarks.md](docs/benchmarks.md#against-etcd).
- `quepaxa`: a follower whose hedged batch loses its slot forwards the entries to the current
  leader again (hedging them anew if it stays silent) instead of proposing them slot after
  slot against a live leader, where they could wait seconds for a random win. Seen after a
  leader fault: the new leader filled every slot while a follower's hedged writes waited
  up to 10 s.
- Verification: a Verus proof that the durability gate never lets a send or effect overtake
  the writes it depends on and that its persists carry exactly the records written, for
  every sequence of calls the replica makes, on a port of `gate.rs` (`verification/verus/`);
  five seeded bugs must fail it. Run nightly.
- Verification: a Verus proof that commit-time deduplication applies each entry at most once,
  incarnation eviction included, on a port of `dedup.rs`; four seeded bugs must fail it.
- Verification: a Verus proof of the window rule in the leader schedule's code: feeding a
  decided slot never changes the order of a slot within `LAG` of it (a port of
  `schedule.rs`; three seeded bugs must fail it).
- `quepaxa`: a snapshot whose leader schedule could not result from the slots it covers is
  rejected (`InvalidCoreState`, or a corrupt record on restart); such a schedule let a
  membership change replace members inside the window. A durable decision beyond the last
  valid slot is now a corrupt record on restart, as it already was over the network.
- `quepaxa`: a batch or snapshot carrying the entry id `u64::MAX` is rejected as corrupt;
  a member sending one could otherwise wrap a dedup watermark and re-apply entries.
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

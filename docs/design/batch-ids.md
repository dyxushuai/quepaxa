# Batch IDs: agree on references, not contents

Status: **proposed** (design only, nothing implemented). Tracking: #172. Related: #155
(saturation profile, a prerequisite), #152 (bulk transfers off the consensus link).
Paper: *QuePaxa*, SOSP 2023, §6.1. Line numbers below refer to the extracted paper text.

## Decisions

1. **Submitter = origin replica.** The replica a client writes to (the entry's `origin`)
   sends a large chunk of its entries, a *data batch*, to every member itself. It sends only a
   reference to the leader (or to the deputy it hands entries to). Clients do not change.
2. **ID = `(origin, incarnation, seq)`**, the crate's existing `ValueKey` shape, plus the
   batch's length and CRC-32C as checks, not as identity. No cryptographic hash.
3. **References are items of the slot value.** The batch encoding (`batch.rs`) gains a
   `Ref` item next to `App`, `Barrier` and `Reconfigure`. A slot value mixes inline items
   and references. Control entries (barriers, membership changes) are always inline.
4. **Content rule (R).** A recorder applies `record(s, v)` that *changes* its register only
   while it holds the content of every reference in `v`. It writes that content as a durable
   `Data` record before the `Recorder` record. Until then the request is parked and gets
   no reply. A request that would not change the register is answered at once.
5. **Proposer rule (P).** A proposer sends record requests for `v` only while it holds the
   content of every reference in `v`. If it adopts a value whose content it lacks, it fetches
   that content from the recorder that reported the value before sending. `SlotProposer` and
   `Register` stay unchanged: both rules live in the replica layer.
6. **Fetch** goes to the requester first (rule P makes it a holder), then rotates through
   the origin and the slot's other members, once per `retransmit_interval`, with at most one
   fetch outstanding per ID. No timer affects safety.
7. **Per origin, by size.** `Config::inline_batch_bytes` (per replica, not cluster-wide): a
   forwarded chunk up to this size travels inline, as it does today, and a larger one goes by
   reference. `u32::MAX` is today's behaviour. It stays the default until the measurements in
   Phase 3 justify another value.
8. **`Decided` carries no value** (Phase 1, which is useful on its own): a receiver takes the
   value from its own register when the keys match, and otherwise catches up.
9. **Applied prefix waits for content; decided prefix does not.** `committed` (leader
   schedule, window, membership) advances on the inline part alone. `emitted` (commits, reads)
   waits until the content is present. Retirement and catch-up use a new *held* prefix:
   decided, with all content durable.
10. **Formats break**: wire and record versions bump to 2, with no compatibility path.

## What the paper says

- §6.1, the mechanism: "After a submitter broadcasts a batch of commands to all replicas in
  QuePaxa, it sends only a small unique batch ID (e.g., a cryptographic hash) to the
  consensus layer" (974-977). "If the recorder has not yet received the batch, it first
  requests the batch from any proposer before responding" (983-984). When the content is
  present, "the recorder responds immediately according to algorithm 2" (981-982).
- §6.1, the motivation: the leader "must send 𝑛 messages and receive up to 𝑛 messages per
  commit, even on the fast path" (965-966). The section relies on the LAN property that
  recipients of a broadcast "usually receive 𝑚 at almost the same time" (972-974).
- §3.1: "Submitters send commands to all replicas, so submitted commands cannot starve" (243-245).
  Footnote 1: "A standard optimization to reduce bandwidth and load is for submitters to
  contact only the current leader first" (247-249). §3.3: "Proposers may batch concurrent
  submitters' commands (or batches) together into one slot" (296-297).
- §7.2: the LAN gain (584k against 400k cmd/s) is attributed "to the optimization discussed in
  Section 6.1, using submitter-driven batch dissemination to reduce critical-path bandwidth"
  (1083-1086). §7.3: throughput falls 20% from 3 to 13 replicas because "the current leader in
  QuePaxa must exchange an increasing number of messages with non-leader replicas"
  (1196-1199). §7.1: those runs use 17-byte commands.
- §3.2: faults are crashes, not Byzantine (239-241), and links are reliable with eventual delivery (266-268).

The paper never says what a recorder does if no proposer answers its request for the batch,
and it says nothing about durability. Both are left to this design.

## What the crate does today

- **Batching.** A follower forwards its pending entries to its target, the leader or a
  deputy, as `Body::Forward { batch }` (`forward_pending`, `replica/proposing.rs`). The
  leader cuts one slot value from its own and relayed entries (`cut_batch`, `new_batch`),
  keyed `ValueKey { origin: me, incarnation, seq }` (`proposal.rs`). That key "identifies
  exactly one value", and `Proposal` equality ignores the bytes.
- **Leader egress for B bytes of slot value.** `RecordRequest` carries the full value to each
  of the n−1 recorders (`send_requests`). `learn(.., broadcast = true)` then sends
  `Decided { proposal }` with the full value to everyone again (`replica/log.rs`). Replies
  are already small: `reply_proposal` elides a value whose key matches the request's
  (`codec.rs`, tag `ELIDED`). Total: about **2(n−1)·B** per slot from the leader, against
  B/n per follower forward.
- **Durability.** `on_request` writes `Record::Recorder` with the full value, and the reply
  leaves through `send_gated` after it is durable (ADR 0002). `learn` writes
  `Record::Decided` with the full value, and commits are applied before that record is
  durable (`docs/benchmarks.md`, #50). The register is dropped at `Effect::DecidedDurable`.
- **What needs the value.** `advance_committed` reads `batch::changes(&value)` to find
  membership changes. `emit_commits` decodes the entries. `on_catch_up` serves full values in
  `Entries`. `checkpoint` rewrites registers and decided slots at or above the floor
  (`replica/snapshot.rs`).
- The wire already reserves a value tag (`VALUE_INLINE` in `put_value`), but this design
  does not use it: references sit at item granularity so that one slot can combine batches
  from several origins (decision 3).

## Design

### Dissemination

When `forward_pending` cuts a chunk larger than `inline_batch_bytes`, the origin:

1. assigns `id = (me, incarnation, next_data)` and computes `crc32c` over the encoded chunk
   (the same `batch::encode` format);
2. sends `Data { id, bytes }` to every replica in `schedule.everyone()` (the current and any
   pending epoch, as `Decided` is broadcast today), its target included;
3. then sends `Forward` to the target with a `Ref { id, len, crc }` item in place of the
   entries. Smaller chunks stay inline in the same `Forward`.

On one ordered link (kv-tokio today), `Data` reaches the target before the `Forward` that
names it. The leader treats a received `Ref` like a relayed entry: it queues it and puts it
in the next slot value it cuts. By rule P, it first fetches content it lacks, which happens
only if the links reorder (for example a separate bulk link, #152). The leader's own clients'
entries stay inline. Its requests already send them once to each recorder, which is exactly
the cost of disseminating them.

Hand-offs reuse the same path without resending content. A suspecting follower hands `Ref`s
to the deputy (`watch.rs`), which already has the data. A hedging origin proposes its own
`Ref`s. `lost_slot` and `hand_on_unstarted` requeue `Ref` items like entries. Commit-time
dedup works on the entries inside each data batch, so a batch included twice is harmless,
as a doubly forwarded entry is today.

Bound: the encoded entries of a slot, inline items plus the `len` of every reference, stay
within `max_batch_bytes`. Commits, `Entries` pages and `max_message_bytes` are therefore
bounded exactly as today. Raising that bound is a separate question.

### The ID

`DataId = (origin, incarnation, seq)` is unique by construction. The incarnation is durable
before anything is sent (`docs/durability.md`, canary `send-before-incarnation-durable`), and
`seq` counts up within an incarnation. It costs no CPU, it is 20 bytes, and it fits the keys
the crate already orders and deduplicates by. A `Ref` also carries `len` (needed for the bound
above and for fetch budgeting) and `crc32c`. A receiver checks both against the content and
rejects a mismatch as a malformed message. This turns the one known way to break uniqueness,
an identity restarted on an empty disk (already listed under "Things that break safety"),
into a detected fault instead of divergent commits.

### Safety: content before record

Agreement does not depend on content: the consensus core decides keys, and slot values are
still inline and small. The risk is a decided slot whose content no live replica holds.
Rules R and P exclude it.

- **R, at the recorder** (`on_request`). Add `Register::would_change(s, v)`, true exactly when
  `record(s, v)` returns true (`s > S`, or `s == S` and `v` above `agg`). If it would change
  and some reference of `v` is missing from the local store, park `(slot, from, step, v)`,
  fetch, and send nothing. When the content arrives, run `on_request` again from the top, so a
  request made stale in the meantime gets the ordinary stale reply. A request that would not
  change the register is answered at once: the reply reports only values already in the
  register, which hold content by induction.
- **Durability ordering.** Rule R writes the `Data` record, if it is not yet written in this
  incarnation's view, through the gate *before* the `Recorder` record. Promise 1 of the
  durability guide (records are durable in order) and `send_after_durable` then give: a reply
  that reports `v` leaves only after the content of `v` is durable at that recorder. No new
  gate mechanism is needed. The Verus gate proof covers it as it stands, since a `Data`
  record is just another write.
- **Why this is enough.** A decision of `v` (fast path at step 4, or phase 2) needs a
  majority of recorders whose replies report `v` from their registers. By R, each of them holds
  the content of `v` durably. So a decided slot always has durable content at a majority of its
  members, and a crash keeps it.
- **Rule P** is what keeps fetches answerable. The requester of a parked request is a holder
  for as long as it is alive. If it dies, its request no longer matters. P costs a fetch only
  when a proposer adopts a foreign value it never received (phase 0 or 3 of a contended round,
  or catch-up to a later step), and it never costs one on the fast path. It also makes every
  decider a holder, so a receiver of `Decided` fetches from its sender first.
- **The ISR is unchanged.** Parking delays a message, which the asynchronous model already
  allows. The recorder applies Alg. 3 to whatever its register is when the content arrives.
  `SlotProposer::on_reply` sees the same replies, only later. Proposer-side holds happen
  between `Progress::Advanced` and `send_requests`, so they look like a slow network.

### Restart, snapshots, compaction, GC

- **Restart.** Replay restores the data store from the `Data` records. A `Recorder` record
  whose references are not all restored before it breaks promise 1, and `Replica::new` rejects
  it with `CorruptRecord`. A `Decided` record may legitimately lack its data (learned, not yet
  fetched). The replica fetches it after restart and holds back `emitted` until it arrives.
- **Checkpoint** (`replace: true`) rewrites every `Data` record still referenced by a retained
  register or a decided slot at or above the floor, ahead of the records that reference it.
  Data referenced only below the new floor is gone with the slots, and the snapshot covers it.
  A fetch for such data is answered with `Compacted { through }`, so the existing
  `SnapshotNeeded` path takes over.
- **What may be evicted.** The store may drop any batch that nothing references: no retained
  register, no decided slot at or above the floor, no local proposer's working value, and no
  queued `Ref` (`pending`, `relayed`, `forwarded`, `batches`). Received but unreferenced
  batches are capped in bytes per origin, sized from `max_batch_bytes` and
  `max_in_flight_slots` (exact value set in Phase 2), and the oldest go first. Eviction is
  always safe: R and P never rely on unreferenced data, and an evicted batch is fetched again
  if it is ever needed. An origin keeps its own batches until all their entries have committed.
- **Disk volume** drops on the fast path. Today a replica writes about 2·B per slot (the value
  in `Recorder`, then in `Decided`). With references it writes B once in `Data`, plus small
  `Recorder` and `Decided` records. The write is lazy, at the first record, decision or
  proposal that references the batch, and it lands in the same persist, so it adds no fsync.

### Catch-up, reads, membership

- **Catch-up.** `Entries` carries each slot's value and the content of its references, so a
  page is self-contained. It is served only up to the *held* prefix. A replica stores the
  content it receives in an `Entries` page before applying it.
- **`committed` and `emitted`.** Control entries are inline, so `advance_committed`, the leader
  schedule, the window rule and membership changes never wait for content. `emit_commits`
  stops at the first slot whose content is missing and fetches it. Linearizable reads already
  wait for `emitted >= committed` (`check_reads`), so they correctly wait for content too. A
  parked request is not recorder state, so `high_water` ignores it. That is safe: a decided
  slot is recorded by a majority, and their registers or logs count it.
- **Retirement.** The envelope's `commit_index` feeds `note_progress`, and `Entries` replies
  are gated so it is durable progress. It becomes the *held* prefix: decided, with every
  reference's content durable. Otherwise a removed replica could retire while the new members
  hold keys but not content. Progress hints use the same field, and in normal operation held
  equals committed.
- **Epoch boundaries.** Data goes to `schedule.everyone()`. A slot in a new epoch can
  reference data that a new member never received: it fetches, like any other miss. A joining
  replica's `Data` and `Fetch` requests are accepted like `CatchUp` (the `request` list in
  `on_message`).

### Liveness

- **Fetch** sends `Fetch { ids }` to one peer. The answer is a `Data` message per id held, or
  `Compacted`. The first target is the requester, for a parked request, or the sender, for a
  `Decided` or an adopted value. Retries go every `retransmit_interval` to the origin, then to
  the slot's members in order. At most one fetch is outstanding per id, and parked requests are
  at most one per (slot, requester) inside the existing recorder window, so memory stays
  bounded.
- **Why it terminates.** A decided slot's content is durable at a majority of its members
  (above), so with at most a minority of every member set lost (the membership guide's fault
  bound), a correct holder exists. Rotation reaches it, and eventual delivery (§3.2) brings
  the content back. A parked request whose requester dies is dropped once the slot is decided
  or a newer request from the same requester replaces it. Nothing is blocked meanwhile: other
  slots and other requests carry on.
- **Lost dissemination** costs one fetch at each replica that missed the `Data`, and only if
  that replica is needed: a recorder outside the fast quorum delays nothing.

## Verification

What does not change: QuePaxa.tla and its TLAPS proofs (G1), because rule R only strengthens
the enabling condition of `Record`. Every behaviour of the new system, with fetch steps as
stuttering, is a behaviour of the old one, so Agreement carries over. Trace validation,
`slot_sim`, and the Kani R1–R6 proof are also unchanged, since `Register` and `SlotProposer`
stay the same. So are G1, G3, G4 and G5 as stated. The Verus proofs of the gate, dedup and
the schedule stay too: `Data` is an ordinary write, dedup is per entry, and control entries
are inline. Their seeded bugs must stay caught.

What changes or is new:

| Item | Change |
|---|---|
| **G6 content custody** (`crates/quepaxa-sim/src/invariants.rs`) | (a) Every value in a replica's register has all its references in that replica's *durable* data, read from the simulated disk. (b) Every decided slot has its content durable, or covered by the replica's floor, at a majority of its members. Checked after every event in the simulator and the model checker. `Inspection` gains the durable data ids. |
| Liveness oracles | The existing "every write served once nothing is left to run", the progress bound and convergence after heal now also require `emitted == committed` on every live replica, which covers fetch. |
| Model-checking scenario (the issue's acceptance) | Three replicas. A follower's `Data` to one recorder is lost (forced deviation). The slot is decided by the other two, then the origin crashes. The third replica must fetch and apply. A variant crashes the requester while a recorder is parked. Both run within the per-PR deviation budget. |
| Simulator | `inline_batch_bytes` drawn per replica per seed (0, small, `u32::MAX`), so mixed clusters run in every hostile, crash-heavy, membership and placement seed. A large-command workload is added. |
| New TLA+ model `verification/tla/Content.tla` | Replicas, lossy dissemination, fetch, the R guard on record, decisions by quorum, crash keeping durable data only, and eviction of unreferenced data. Invariant `DecidedContentDurable` (G6b at model level). Liveness under weak fairness of fetch: every live replica eventually holds every decided slot's content. TLC runs it exhaustively for 3 replicas, 2 batches and 2 slots. G6b is a short inductive invariant, so the plan adds a TLAPS proof (`ContentProof.tla`, listed in `prove.sh`) and two seeded TLA bugs (record without the guard, eviction of referenced data). |
| `Membership.tla` | The spec is unchanged. The table row "`Learn` / `dlog`" now maps to the *held* prefix, not to `committed`. `Recoverable`'s register branch relies on G6a. Re-check that the retirement code uses held. |
| `Reads.tla` | Unchanged. Its `Prefix` is already the applied prefix (`committed == emitted`). Add a table note that a parked request is not a register. |
| Kani | A new harness: `would_change(s, v) == record(s, v)` from every register state. If they differed, the recorder would either reply without content or wait for nothing. |
| Fuzzing | `Ref` items in `batch::decode`, the `Data` and `Fetch` messages, `Entries` with content, `KIND_DATA` records. |

New canaries (`verification/canaries.tsv`):

- `record-without-content`: `on_request` skips the content check. G6a must fail (model check).
- `data-after-register`: the `Data` record is written after the `Recorder` record.
  `replica_sim crash_heavy` must fail G6a with torn writes.
- `checkpoint-drops-data`: the checkpoint omits referenced `Data`. `crash_heavy` must fail
  G6 or a commit oracle.
- `evict-referenced`: eviction ignores register references. G6a must fail.
- `fetch-once`: no retry or rotation. The new model scenario must report a write not served.
- `retire-on-committed`: retirement counts `committed`, not held. Needs a targeted membership
  simulation (removal while a new member lacks content, then old-member crashes). If that
  test cannot reach it, the canary is caught by a unit test and the gap is recorded, as for
  the existing "membership change applied twice".

## Performance

Leader egress per slot carrying B bytes of client entries spread evenly over n origins:

| | Leader egress | Each follower's egress |
|---|---|---|
| Today | 2(n−1)·B | B/n |
| Phase 1 (`Decided` without value) | (n−1)·B | B/n |
| Phase 2 (references, all forwarded chunks by reference) | (n−1)·B/n + refs | (n−1)·B/n |

For n = 3 that is 4B, then 2B, then 0.67B. Egress becomes even across replicas, and total
bytes on the network fall too. If egress is the limit, the ceiling rises by up to 2n. If
CPU, disk or syscalls are the limit, it does not rise at all. This explains the §7.3
replica-count effect.

**When it wins:** a LAN with large forwarded chunks (KiB to MiB), more replicas, and links
whose bandwidth is the limit. Clients must write to followers: if every client writes to the
leader, there is nothing to forward and nothing gained.

**When it loses:**

- (a) The WAN. A recorder needs a fetch when the leader's request arrives before the
  origin's direct `Data`. That happens when the path through the leader is not slower
  (geography rarely gives this, but n−1 copies queued on the origin's uplink and TCP loss
  recovery do). The fetch adds a round trip to the requester, plus the transfer, to that
  recorder's reply. It delays the commit only if that recorder is in the fastest majority.
- (b) Small chunks: a reference costs about as much as the entries, which is why chunks up to
  `inline_batch_bytes` stay inline.
- (c) CPU at followers, which now encode n−1 copies (`Message::encode` copies the payload per
  destination, since the envelope names `to`). Sharing the payload across encodings is a
  separate codec optimization.

**Configurable, not adaptive (yet).** Each origin decides per chunk by size, and mixing is
safe, so no cluster-wide agreement is needed. Operators set `inline_batch_bytes = u32::MAX` on
replicas across a WAN. An adaptive rule (by observed fetch rate) needs feedback the origin
does not have today, because fetches are served by others. It is deferred until Phase 3 data
shows a need.

**Measure first.** The existing in-memory runs saturate near 200k ops/s with 17-byte
commands. At about 50 bytes per entry, that is roughly 40 MB/s of leader egress on loopback
with no bandwidth limit. Egress is unlikely to be the limit there, so this change should not
be expected to move those numbers. #155's profile must first show where the replica thread
saturates. The benchmark has to make bandwidth matter.

**Benchmark acceptance:**

- **LAN.** `run-local.sh` gains a command-size option and a per-replica egress limit (for
  example `tc` with a 1 Gbit/s rate on each replica's traffic). It runs 3 and 5 replicas with
  17-byte, 4 KiB and 64 KiB commands, before and after each of Phases 1 and 2, on disk and in
  memory. Each run records saturation throughput, p50/p99 at a fixed sub-saturation rate, and
  leader egress bytes per slot (new metrics). Proposed bar:
  - 4 KiB and 64 KiB commands: at least 2x saturation throughput at n = 3;
  - fixed-rate p50 no worse than 10%;
  - 17-byte commands: no worse than 5%.
- **WAN.** The emulated-region harness (`examples/kv-tokio/bench/etcd/wan.sh`, "Across regions" in `docs/benchmarks.md`: five regions, 200 puts/s per region, medians
  of three) with references on, at 17 bytes and at 4 KiB. p99, longest gap and errors must be
  within the existing run-to-run range in every fault row, and the fetch count is recorded.
  If it regresses, references stay off for WAN deployments and `tuning.md` says so.
- Results go in `docs/benchmarks.md`. The §6.1 row in `docs/paper-conformance.md` becomes
  Implemented.

## API, wire, storage, hosts

- **API.** `Config::inline_batch_bytes: u32` (default `u32::MAX`, validated at least 64 and
  ignored above `max_batch_bytes`). New `Metrics`: `data_sent`, `fetches`, `fetch_retries`,
  `fetches_served`, `parked_requests`. `propose`, `Action::Commit`, `Outbound` and the
  snapshot and `core_state` formats are unchanged.
- **Wire, `WIRE_VERSION` 2.**
  - `Decided { slot, priority, key }` (Phase 1).
  - New kinds `Data { id, bytes }` and `Fetch { ids }`.
  - `Entries` gains a content section.
  - Batch item kind `ENTRY_REF` carrying `(origin, incarnation, seq, len, crc32c)`.
  - Golden-bytes tests are regenerated.
- **Storage, `FORMAT_VERSION` 2.** New `KIND_DATA` record. `Recorder` and `Decided` records are
  unchanged in shape, but their values may hold references. Old records are refused, as the
  durability guide already states for format changes.
- **kv-tokio.** No change to how it moves bytes. It gets a flag for the new option and the new
  metrics in `STATS`. Its bounded per-peer queues may drop `Data` frames, and fetch recovers
  them. If #152 puts bulk frames on a separate link, `Data` and its `Forward` lose ordering,
  and rule P handles that. #152 should then classify `Data` and `Entries` as bulk. That needs
  a message class on `Outbound`, which is #152's decision, not this one's.
- **C ABI.** `QPX_CONFIG_INLINE_BATCH_BYTES`, the new metric fields, a regenerated header and
  `symbols.txt`. The smoke test and Miri suite are unchanged.

## Rejected alternatives

- **Cryptographic hash IDs.** Hashing costs CPU on the single replica thread on every send and
  receive. It brings a crypto dependency into a `no_std` core (supply-chain surface). It also
  defends against Byzantine senders, who are outside the threat model (§3.2). The key is
  already unique, and the CRC catches the misconfiguration case.
- **Clients broadcast to every replica** (the paper's literal submitter). This changes the
  client protocol of every host, and the hedging and forwarding machinery already plays the
  footnote-1 role. The origin replica is the front-end proxy that §3.1 (235-238) describes.
- **Record IDs without content, fetch only at apply.** A decided ID could then have no
  surviving content after a crash of the origin, which is exactly what the issue rules out.
- **One data batch per slot** (a `Proposal` value that is a single ID). It needs a slot and a
  consensus instance per origin chunk. §3.3 lets proposers combine submitters' batches.
- **The reserved `VALUE_INLINE` tag for a whole-value reference.** It cannot mix origins or
  keep control entries inline.
- **Eager `Data` writes on receipt.** They add disk writes for data that may never be
  referenced, and gain nothing: the lazy write lands in the same persist as the record that
  needs it.
- **Fetch from the origin first.** The origin is the natural spare-egress holder, but rule P
  guarantees only the requester. Trying the origin second keeps the first attempt certain.
- **Waiting a grace period before fetching** (in case the `Data` is in flight). This is an
  open question for the WAN, not a default.

## Implementation plan

Each phase is one PR that passes the full per-PR suite and the canaries.

- **Phase 0, measure.** #155 profile, plus the bandwidth-limited, large-command LAN harness
  and its baseline numbers in `docs/benchmarks.md`. Go/no-go: proceed past Phase 1 only if
  leader egress (or its syscalls) shows up as the limit with large commands.
- **Phase 1, `Decided` without value.** The receiver resolves the value from its register's
  `first`, `agg` or `prev_agg` by key, else requests catch-up for that slot.
  - Tests: codec golden bytes; a unit test for a receiver without the register; existing
    simulator and model checking.
  - Benchmark: leader egress per slot and LAN throughput before and after.
- **Phase 2, references, off by default.**
  - Code: `Ref` items, the data store with its eviction rules, `Data` and `Fetch`, rules R
    and P, `KIND_DATA` and checkpoint rewrite, content in `Entries`, the held prefix for
    retirement and progress.
  - Tests: `would_change` Kani proof and property test; batch, codec and record fuzz targets;
    restart with a `Recorder` record before its `Data` (`CorruptRecord`); G6; the
    model-checking scenarios; the randomized simulator option; `Content.tla`, its proof and
    seeded bugs; the six canaries.
  - Docs: `protocol.md` (around the core protocol), `durability.md`, `snapshots.md` and
    `tuning.md` (when to turn it on), and the VERIFICATION.md layers and model-to-code table.
- **Phase 3, decide the default.** LAN and WAN benchmarks as above. Set the default
  `inline_batch_bytes` (a LAN-friendly value if the WAN shows no regression with it, else keep
  `u32::MAX` and document the per-deployment choice). Update the paper-conformance row.

## Open questions for the maintainer

1. **Default.** Keep the default `u32::MAX` (off) permanently and document it as a LAN
   option, or make it on once the benchmarks pass?
2. **Fetch grace on the WAN.** Fetch at once (the paper's rule, and the simplest), or wait a
   fraction of the requester's round trip for an in-flight `Data`? Decide on Phase 3 data.
3. **Slot size bound.** Keep the referenced bytes inside `max_batch_bytes`, or add a separate,
   larger bound for content? The latter needs paged commits and `Entries`.
4. **Phase 1 alone.** If #155 shows CPU, not egress, as the limit, is Phase 1 enough for
   0.8.0, with Phase 2 deferred?
5. **The CRC in `Ref`.** Keep it (about 4 bytes and one pass over the data, catching a reused
   identity), or drop it as redundant with record checksums?

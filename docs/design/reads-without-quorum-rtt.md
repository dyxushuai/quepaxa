# Reads without a quorum round trip (#158)

Status: **No-Go** (design decision recorded in [ADR 0006](../adr/0006-no-zero-rtt-linearizable-reads.md)).
Tracking: #158 (`v0.10.0`). Related: [ADR 0003](../adr/0003-quorum-reads.md),
[ADR 0004](../adr/0004-what-carries-over-from-raft.md), [design matrix](axiomatic-design.md),
`verification/tla/Reads.tla`, `verification/tla/ReadsProof.tla`.

## Problem

Every linearizable read in `quepaxa` currently calls `Replica::read_barrier`, which runs one
majority round trip (`ReadQuery` / `ReadReply`, PQR, ADR 0003) to learn the high-water mark
`K` and waits until the local replica has applied every slot below `K`. Concurrent reads
coalesce onto a single round and nothing is written to disk, yet a single unbatched
linearizable read still pays one RTT to the closest majority of voting members.

Issue #158 asks whether `quepaxa` can serve linearizable reads without that majority round
trip, under the project's constraints:

- **No exclusive leader:** any QuePaxa replica may propose and decide any slot (hedging,
  step-aside, randomized slow path). Raft's leader-lease read does not apply (ADR 0004).
- **No clock-bound safety:** `docs/design/axiomatic-design.md` mandates that safety
  (FR1–FR4) never reads a timing parameter; #158 explicitly puts clock-bound leases out of
  scope.
- **Liveness without timeouts (FR5):** a minority of crashed or partitioned replicas must not
  stall cluster writes (`f < N / 2`).

## Candidate mechanisms

### Option A — Clock-free revocable quorum leases

A reader `r` acquires a promise from a majority `Q` of recorders in epoch `E`: *"I will not
record any slot `>= k` until `r` acknowledges revocation of this promise."* While `r` holds
promises from a majority `Q` and has applied `< k`, no quorum can decide any slot `>= k`, so
`r` can answer linearizable reads locally with 0 RTT.

When any proposer wants to write slot `>= k`, recorders in `Q` must send `Revoke` to `r` and
wait for `RevokeAck` from `r` before accepting the write.

**Why this fails (asynchronous crash/partition impossibility):**

Consider two executions indistinguishable to the voting majority `Q`:

1. `r` holds a valid quorum promise from `Q` at slot `k` and is **partitioned** (still alive,
   receiving client reads).
2. `r` holds a valid quorum promise from `Q` at slot `k` and has **crashed permanently**.

Without a timing assumption:
- If `Q` ever breaks the promise without `RevokeAck` from `r` and decides slot `k`, then in
  execution (1) `r` continues answering zero-RTT linearizable reads at `< k` after slot `k`
  is decided, **violating linearizability (FR2)**.
- If `Q` waits for `RevokeAck` before recording slot `k`, then in execution (2) no slot
  `>= k` — neither application writes nor a membership reconfiguration to remove `r` — can
  ever be decided, **halting the entire cluster on a single reader crash (violating FR5)**.

Restricting promises to a subset of keys does not help in `quepaxa`: the consensus core
agrees on an opaque byte log (`Slot -> Proposal`) and has no key-space visibility. Every
write and every reconfiguration enters the same slot sequence.

### Option B — Time-bounded quorum leases (Paxos Quorum Leases)

Each recorder in `Q` grants `r` a promise that expires after `T_lease` on the recorder's
local monotonic clock (Moraru et al., SoCC 2014); `r` serves zero-RTT reads while its own
clock is below `t_request + T_lease - ε` (accounting for bounded clock-rate drift `ρ` and
process pauses).

**Why this is No-Go for `quepaxa`:**

1. **Out of scope and violates the design matrix:** safety (linearizability of reads, FR2)
   now depends on a timing parameter (`DP_lease_clock`, clock drift bound `ρ`, and absence of
   GC/VM/scheduler pauses exceeding `ε`). In `docs/design/axiomatic-design.md`, that puts an
   `X` in row FR2 on a timing DP, breaking *"Safety (FR1–FR4) is decoupled... timers decide
   when a replica acts, never whether the result is correct."* Neither TLAPS nor Verus can
   discharge read linearizability without adding an uncheckable physical-clock axiom.
2. **Re-introduces timeout stalls on the write path:** if a lease holder crashes or is
   partitioned, writers stall for up to `T_lease` waiting for the lease to expire before
   recorders can accept writes again. That trades away QuePaxa's defining WAN property
   (liveness without timeout-length stalls, `STRATEGY.md`) for read latency, and makes
   write availability depend on every lease-holding reader (including observers!).
3. **Worst-case latency inversion under mixed workloads:** any write requires revoking
   active leases (an extra RTT to every lease holder before recording), turning a 1-RTT
   fast-path write into a 2-RTT write whenever any replica holds a lease.

### Option C — All-write / local-read quorums (`|Q_W| = N`, `|Q_R| = 1`)

Require every write to be recorded by all `N` voting members so every member intersects every
write quorum and can serve 0-RTT linearizable reads locally (as in CRAQ or FPaxos with
`Q_W = N, Q_R = 1`).

**Why this is No-Go:**

Any single slow, restarted, or crashed member immediately stalls all writes (`0` write fault
tolerance). Recovering requires reconfiguring the slow member out, which itself cannot be
decided under `Q_W = N` without an external configuration master or a timeout-based fallback
to a majority quorum (re-introducing either a single point of failure or Option B).

## Axiomatic design check

| Candidate | Safety (FR1–FR4) | Liveness (FR5) | Latency (FR6) | Fits QuePaxa? |
|---|---|---|---|---|
| A. Clock-free revocable quorum lease | Holds | **Broken** (1 crashed reader halts writes forever) | 0 RTT reads; 2 RTT writes while leased | **No** |
| B. Time-bounded quorum lease | **Coupled to clock drift & pauses** | **Degraded** (write stalls up to `T_lease` on reader crash) | 0 RTT reads; 2 RTT writes while leased | **No** |
| C. All-write quorum (`Q_W = N, Q_R = 1`) | Holds | **Broken** (0 write fault tolerance) | 0 RTT reads; writes wait on slowest of `N` | **No** |
| D. Existing PQR (`ADR 0003`) + observers (`#157`) + host-side session/stale reads | **Decoupled** (`ReadsProof.tla`) | **Timeout-free** (`f < N/2`) | 1 RTT to closest majority (coalesced); 0 RTT for stale/session reads | **Yes** |

## What the engine already gives services

Without compromising FR1–FR5, services built on `quepaxa` already have four tools to control
read latency:

1. **Closest-majority PQR (`Replica::read_barrier`):** any voting member or local non-voting
   observer (`#157`) queries all voting members and completes once the fastest majority
   (`f + 1`) responds. It never routes through a distant leader and never waits on the
   farthest minority.
2. **Automatic read coalescing:** concurrent `read_barrier` calls while a round is in flight
   share the next round, amortizing the majority RTT across bursts of reads.
3. **Zero-RTT local reads with bounded staleness:** reading local applied state directly
   costs 0 RTT. Paired with an observer replica in the reader's region (`Config::follow_interval`,
   `#157`), normal-case staleness is bounded by `follow_interval + 0.5 RTT` to the nearest
   voting member.
4. **Zero-RTT read-your-own-writes (host-side session pattern):** a service can return the
   committed `Slot` from `Action::Committed { id, slot }` as a session token; subsequent
   reads in that session read local state immediately once `commit_index() > slot` (or wait
   for `follow` / issue a `read_barrier` only if behind). This needs no consensus protocol
   changes.

## Decision

**No-Go** on implementing zero-RTT linearizable reads in `quepaxa`.

- No consensus-core protocol change or lease implementation will be added for #158.
- Linearizable reads remain PQR majority rounds (`ADR 0003`), already verified by
  `Reads.tla` (fixed membership, membership changes, and non-voting observers) and proved in
  `ReadsProof.tla`.

## Revisit conditions

Reopen only if both hold:
1. A real multi-region workload demonstrates that coalesced closest-majority PQR plus local
   observers cannot meet its read-latency SLO, **and**
2. The project explicitly decides to allow an opt-in clock-bound extension (relaxing the
   *"safety never reads a timing parameter"* rule for lease-enabled clusters) with a
   separate proof over an explicit clock-drift model.

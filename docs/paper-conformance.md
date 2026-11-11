# Paper conformance

How this crate implements *QuePaxa: Escaping the Tyranny of Timeouts in Consensus*
(Tennage et al., SOSP 2023), mechanism by mechanism. Each row is one of:

- **Implemented**: matches the paper; the evidence column names the code and its checks.
- **Extension**: something the paper leaves open (reconfiguration, reads, recovery details), or a
  refinement layered on a paper mechanism without changing it. `crates/quepaxa/docs/protocol.md`
  ("What the paper leaves open") describes each one.
- **Deviation**: differs from the paper. Every deviation has an issue that either removes it or
  records why it stays.
- **Missing**: in the paper, not in the crate yet; the issue tracks it.

Last checked against the paper on 2026-11-08.

## Consensus core (§3.3, §4)

| Paper | Mechanism | Status | Evidence |
|---|---|---|---|
| §3.1, Fig. 1 | Every replica runs a proposer (active) and a recorder (passive); RPC-style, proposer to recorder only | Implemented | `crates/quepaxa/src/proposer.rs`, `recorder.rs` |
| §3.3, §4.2.2 | Step = 4·round + phase; a step advances once a majority answered the previous one (threshold logical clock) | Implemented | `proposer.rs` (`FIRST_STEP`, `advance`) |
| §4.1 | Abstract protocol on `tcast` (Alg. 1): propose, propagate existent sets, propagate common sets, decide when best(E) = best(U) | Implemented, as the paper does it, through the concrete protocol | `QuePaxa.tla` models one slot of the paper's algorithms apart from the code |
| §4.2.2–4.2.3, Alg. 2–3 | Interval summary register: step S, first value Fc, current aggregate Ac, prior aggregate Ap; integer maximum as aggregate, nil as base; stale steps discarded; a skipped step clears Ap | Implemented | `Register::record`; Kani proof of R1–R6 over every state (`recorder.rs`) |
| §4.2.4, Alg. 4 | Proposer phases: phase 0 adopts the best first value of a quorum; phase 1 spreads it; phase 2 decides when it is the best prior aggregate; phase 3 adopts the best prior aggregate | Implemented | `SlotProposer::on_reply`; trace validation against `QuePaxaTrace.tla` |
| §4.2.4 | Catch-up: a reply from a later step makes the proposer jump to it with that reply's first value | Implemented (on the first such reply, as the prose says; the pseudocode checks after a quorum) | `SlotProposer::on_reply` |
| §4.2.4, App. A.1 | A fresh random priority per recorder in phase 0; ties broken by appending the proposal's identity | Implemented: 64-bit ChaCha20 priorities, ties ordered by `(origin, incarnation, seq)` | `rng.rs`, `proposal.rs` (`Ord`) |
| §4.2.5 | Fast path: only the slot's leader, only in round 1, uses the reserved top priority H; it decides in phase 0 when every reply in a quorum carries H first | Implemented; a restarted leader reserves H durably so it never uses it twice in a slot | `proposer.rs` (debug assertion P3); `replica/` |
| §4.2.5 | Rounds 2 and later are leaderless and fully asynchronous | Implemented | `prepare` randomizes every phase 0 after round 1 |
| §4.2, footnote 5 | Bounded step numbers | Implemented (`MAX_STEP`) | `types.rs` |
| App. B–C | Safety and liveness proofs | Implemented as machine-checked proofs: TLAPS proof that `QuePaxa.tla` never decides two values, for any number of recorders, proposers and rounds; G1–G5 checked on real replicas in simulation and model checking | `VERIFICATION.md` |
| App. D | Promela models checked with SPIN | Covered by other means: TLA+ models checked with TLC, plus the Rust model checker over the real code | `VERIFICATION.md` |
| footnote 6 | Flexible quorums on the fast path | Not planned: the paper leaves it to future work | — |

## State machine replication (§3.3, §5, §6)

| Paper | Mechanism | Status | Evidence |
|---|---|---|---|
| §3.3 | A log of slots, each decided by its own consensus instance; commands applied in slot order | Implemented | `replica/log.rs` |
| §3.3, §6, §7.1 | Batching at the proposers | Implemented: one batch per slot, up to `Config::max_batch_bytes` | `replica/proposing.rs` |
| §6, §7.2 | Pipelining (the paper uses 10 slots on the WAN) | Implemented: up to `Config::max_in_flight_slots` (default 8, at most `LAG` = 64) | `replica/proposing.rs` |
| §3.1, footnote 1 | Submitters send commands to every replica, or (the footnote's optimization) to the leader first and to others only if it does not answer quickly | Implemented as the footnote's optimization: a replica forwards its clients' commands to the leader and hands them to the next member if they are not committed within the hedge delay | `replica/proposing.rs`, `replica/watch.rs` |
| §4.2.5 | All proposers agree on each round's leader, derived from a prior decision | Implemented: the leader order is part of the decided log | `schedule.rs`; Verus proof of `schedule.rs`, TLAPS proof of the window rule (`MembershipProof.tla`) |
| §5.2 | Hedging schedule: the leader proposes at once, the k-th next proposer after k·δ, each only if it has not seen progress | Implemented: a slot another member started is driven after (position + 1)·δ without progress | `replica/proposing.rs` |
| §5.2 | A single base delay δ | Extension: δ adapts to four times the smoothed commit latency, within `[hedge_min, hedge_max]` | `Replica::hedge_delay`; #173 decides whether to tune it as the paper's bandit |
| §5.3, §1, abstract | Leader choice and hedging schedule tuned as a multi-armed bandit: fixed-length epochs with a stable leader; round-robin exploration (two epochs each) over the first 2n+1 epochs; the schedule sorted by observed average epoch completion time and agreed through consensus; the leader monitored every epoch; exploration again only when it falls behind the next in the schedule | **Deviation**: placement ranks members by a round-trip cost model (`replica/ranking.rs`), which cannot see a slow CPU or disk | #173 replaces it with the paper's mechanism (supersedes the ranking in #147) |
| §5.3, footnote 7 | Restless-bandit re-exploration of non-leaders | Optional refinement in the paper | Considered in #173 |
| — | Rotation after eight consecutive slots not decided by their leader; silent-leader takeover by a deputy; a leader with a stuck disk steps aside | Extension: failure handling the paper leaves to hedging and §5.3's monitoring; none of it affects safety | `schedule.rs`, `replica/watch.rs`; `leader_loss.rs`, `disk_faults.rs` |
| §6.1 | Submitters disseminate batches to every replica; consensus agrees on batch IDs, and a recorder fetches a batch it lacks before answering | **Missing** | #172 |
| §6 | Reconfiguration "via standard practices" | Extension: membership and leader-order changes through the log, with a generation check | `schedule.rs`; `Membership.tla`, `MembershipProof.tla`, Verus |
| §7.1 (Rabia's log compaction) | Log compaction | Extension: snapshots and catch-up | `crates/quepaxa/docs/snapshots.md` |
| — | Durability: recorder state persisted before any reply that reports it | Extension (the paper's crash-stop model needs none) | `durable.rs`; crash-restart in simulation and model checking |
| — | Group commit of persists | Host side (kv-tokio): one fsync per group on a writer thread | `examples/kv-tokio/src/node.rs` |

## Reads

The paper does not cover reads; everything here is an extension.

| Mechanism | Status | Evidence |
|---|---|---|
| Linearizable read at any replica (leader or follower) through a majority's high-water mark, no slot used | Implemented | `crates/quepaxa/docs/reads.md`; `Reads.tla`, `ReadsProof.tla` |
| Local (possibly stale) read | Implemented in the host (kv-tokio `STALE`) | `examples/kv-tokio` |
| Read without a quorum round trip (quorum lease) | Open: design, model and go/no-go first | #158 |
| Observer replicas serving reads in their region | Open | #157 |

## Threat model (§3.2)

| Paper | Requirement | Status | Evidence |
|---|---|---|---|
| §3.2 | Reliable links (TCP) | Host responsibility | kv-tokio uses TCP |
| §3.2 | Content-oblivious adversary: replica-to-replica links encrypted | Host responsibility; the operator guide must say so | #160 |

## Evaluation (§7)

| Paper | Experiment | Status | Evidence |
|---|---|---|---|
| Fig. 6 | LAN and WAN throughput against latency | Partly: LAN saturation and five emulated regions recorded | `docs/benchmarks.md`; #174 |
| Fig. 7 | LAN scalability, 3 to 13 replicas | Missing | #174 |
| Fig. 8 | Adversary delaying a changing minority by up to 500 ms | Partly: fixed slow, jitter and loss faults on the leader's region | `examples/kv-tokio/bench/etcd/wan.sh`; #174 |
| Fig. 9 | Recovery and throughput across hedging delays after a leader failure | Partly: leader crash and partition at fixed delays | `docs/benchmarks.md`; #174 |
| Fig. 10 | Convergence to the best leader among heterogeneous machines | Missing until #173 | #173, #174 |
| §8 | Automated tuning of batch size and pipeline length (related work the paper calls complementary) | Open | #175 |

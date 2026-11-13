# How QuePaxa works

A short tour of the protocol as this crate implements it, for readers who want to know
what the replica is doing. The source is *QuePaxa: Escaping the Tyranny of Timeouts in
Consensus* (Tennage, Basescu, Kokoris-Kogias, Syta, Jovanovic, Estrada-Galiñanes, Ford;
SOSP 2023).

## Roles

Every replica plays two roles for every slot of the log:

- A **recorder** holds one register per slot: the current *step*, the first value it
  recorded at that step, the highest value at that step, and the highest value at the
  previous step. It answers each request with a summary of the register. Recorders are
  passive and stateless beyond the register; their state is what must be durable.
- A **proposer** drives a slot by sending requests to all recorders of the slot and
  advancing through steps when a majority answers. A proposer keeps no durable state:
  safety comes from the recorders.

## Steps and rounds

A slot's clock is the step `4·round + phase`, starting at round 1, phase 0. In each round:

- **Phase 0** — propose: each proposer sends its value with a fresh random priority per
  recorder (the leader, in round 1 only, uses the reserved top priority). From a majority
  of replies it adopts the highest *first* value it sees. If, in round 1, every reply
  carries the leader's top-priority value first, that value is **decided** — the fast path,
  one round trip.
- **Phases 1–2** — spread the adopted value; at phase 2, if the highest previous-step value
  a majority reports is the proposer's own, it is **decided**.
- **Phase 3** — adopt the highest previous-step value a majority reports, and start the
  next round.

Randomized priorities make it likely that one value dominates a round even when several
proposers compete, so the slot terminates with probability 1 — without relying on timeouts
for liveness. Timeouts (hedging) only decide *who* proposes when, to save work.

## Safety in one paragraph

A decision at phase 2 of round *r* means a majority recorded the value as the highest of
step `4r + 1`; any majority intersects it, so every proposer that completes phase 3 of
round *r* adopts that value, and every later round can only carry it (the paper's lemmas;
this crate checks them as invariants G1–G5 on real replicas in simulation and model
checking). The fast path is safe because only one proposer per slot may use the top
priority, and a majority holding it first at step 4 forces every proposer to adopt it.

## Around the core protocol

What the paper leaves open, the crate fixes as follows (the repository's
`docs/paper-conformance.md` checks every mechanism of the paper against the crate):

- **Log.** Each slot is an independent instance; a replica drives up to
  `max_in_flight_slots` at once and applies decided slots in order.
- **Leader.** Derived from the decided log, so it needs no election; it rotates after eight
  consecutive slots it did not lead. A restarted leader reserves its top priority durably
  before using it, so it can never use it twice in one slot.
- **Hedging.** Followers forward entries to the leader and propose them themselves only if
  they are not committed within an adaptive delay; duplicates are filtered when committed.
  A follower whose batch loses its slot forwards the entries to the leader again rather
  than competing with it for the next slot. With `silent_leader_takeover` (the default), a
  leader that says nothing at all for about a commit latency after a forward is suspected
  (it acknowledges forwards at once, before any write, so a slow disk does not make
  it silent): the follower hands its entries to the next member of the leader order,
  which proposes them, so followers do not contend for slots before the schedule rotates,
  and the leader's unfinished slots are taken over as soon. Hearing from it ends the
  suspicion. A leader whose own disk is stuck (a write waiting four hedge delays, while
  others still decide slots) steps aside the same way: it hands its entries to the next
  member and stops acknowledging forwards until its disk catches up; once it decides
  slots of its own again, the next member hands back what it was given rather than lose
  slot after slot to it. A follower whose own
  disk is stuck while nobody decides (a write in flight and none completed for a hedge
  delay: every disk may be stuck, one shared disk) does not hedge, and counts the delay
  again from when its disk catches up: a hedge could not be decided sooner, and would
  contend with the leader's slots, off its fast path, the moment the disks resume. A disk
  that keeps completing writes is slow, not stuck, and does not hold hedges; nor does
  one that would be stuck by the time a hedge is due, until it is.
- **Durability.** Recorder state is written before any reply that reports it leaves.
- **Snapshots, catch-up, membership.** See the other chapters.

### Disk health

Each replica judges its own disk (`replica/disk.rs`) from three events: a write issued, writes
completed, and a slot another replica decided. δ is the hedge delay; *quiet* means no write
completed for max(δ, four usual write times) while one was in flight; *due* is four usual
write times after the oldest write in flight was issued.

| Phase | Becomes | When |
|---|---|---|
| Idle | Writing | a write is issued |
| Writing | Idle | every write in flight completes |
| Writing | Stuck | time passes: quiet, and someone decided after due |
| Writing | SharedStall | time passes: quiet, and nobody decided after due |
| SharedStall | Stuck | a decision arrives after due |
| Stuck, SharedStall | Writing, Idle | a write completes (from SharedStall: δ for held hedges restarts) |

Followers hold hedges only in SharedStall. The replica others hand their entries to steps
aside once its oldest write has waited 4δ (at most `hedge_max`, at least four usual write
times) with a decision since it was issued, and comes back when that no longer holds; the
two alternate.

## Glossary

- **Replica** — one member of the cluster; runs a proposer and a recorder for every slot.
- **Slot** — one position in the log, decided by one consensus instance.
- **Step** — a slot's logical clock, `4·round + phase`.
- **Decided** — a slot whose value can no longer change.
- **Batch** — the value decided in one slot: one or more proposed entries.
- **Leader** — the replica allowed the top priority in a slot's first step.
- **Hedge delay** — how long a replica waits before driving work itself; affects only
  performance.
- **Epoch** — a range of slots with one set of members.

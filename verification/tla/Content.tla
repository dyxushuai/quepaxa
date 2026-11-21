------------------------------ MODULE Content ------------------------------
(***************************************************************************)
(* Batch content behind references (#172, docs/design/batch-ids.md),      *)
(* written apart from the Rust code: slot values name data batches by ID, *)
(* and the content travels apart from consensus.                          *)
(*                                                                         *)
(* An origin creates each batch and sends it as `Data` to every member;   *)
(* any of them may miss it (lossy dissemination). A replica holds content *)
(* in memory (data) and durably (ddata, its `Data` records), and fetches  *)
(* content it needs from a live member holding it. A proposer proposes    *)
(* only a value whose content it holds (rule P). A recorder records a     *)
(* value only while it holds all its content, and writes the `Data`       *)
(* records ahead of the `Recorder` one (rule R: durable in order, so the  *)
(* content is durable when the register is). A slot decides a value a     *)
(* majority of registers hold. Replicas learn decisions (and drop the     *)
(* register), apply and compact, and fetch the content of a decided slot  *)
(* they lack; a member that compacted the slot answers `Compacted` (DEV-3: *)
(* the fetch names the slot) and the fetcher installs its snapshot.       *)
(* Content stays pinned while a register, a decided slot at or above the  *)
(* floor (DEV-4: until compaction), a working value or its origin (until  *)
(* committed) needs it; anything else may be evicted. A crash loses       *)
(* memory (data, working values) and keeps the disk.                      *)
(*                                                                         *)
(* Consensus inside a slot is QuePaxa.tla's. Here a register takes one    *)
(* value per slot and a slot decides once: the recorders of a deciding    *)
(* quorum stay on its value (the lock G3 and G4 give), and the later      *)
(* steps of the others would only add holders and pins.                   *)
(*                                                                         *)
(* Checked: DecidedContentDurable (G6b: every decided slot's content is  *)
(* durable, or below the floor, at a majority) and ContentSpreads (every  *)
(* decided slot's content is eventually held, or compacted, at every      *)
(* replica still running) under weak fairness of learning and of fetching *)
(* from each member, with at most MaxCrash crashes (each a restart) and   *)
(* MaxStop replicas, a minority, stopped for good. ContentProof.tla       *)
(* proves DecidedContentDurable for any number of replicas, batches,      *)
(* values and slots.                                                      *)
(*                                                                         *)
(* Not modelled: membership changes (every replica is a member of every   *)
(* slot; Membership.tla covers changes), steps and priorities inside a    *)
(* slot, entries and their dedup, inline items, catch-up pages, timers    *)
(* and fetch rotation (fairness per member stands for them), batch sizes  *)
(* and the eviction cap, torn writes (the simulator's). run.sh checks     *)
(* that seeded bugs are found.                                            *)
(***************************************************************************)
EXTENDS Integers, FiniteSets

CONSTANTS Replicas,   \* e.g. {1, 2, 3}
          Batches,    \* data batch IDs
          OriginOf,   \* OriginOf[b]: the replica that creates batch b
          Proposers,  \* the replicas that propose
          NS,         \* slots 0..NS-1
          Vals,       \* Vals[s]: the values proposed in s, each the non-empty set of
                      \* batches it references
          MaxCrash,   \* crashes
          MaxStop     \* replicas stopped for good

\* For the configuration (a .cfg file cannot write a function): batch b comes from replica
\* b; slot 0 is contended between the two origins' batches, slot 1 references both.
Orig12 == [b \in {1, 2} |-> b]
Vals2 == [s \in {0, 1} |-> IF s = 0 THEN {{1}, {2}} ELSE {{1, 2}}]

Slots == 0..NS-1
Maj(Q) == 2 * Cardinality(Q) > Cardinality(Replicas)

\* The empty set stands for no value: an empty register, no decision, no working value.
VARIABLES created,  \* batches their origin created
          data,     \* data[r]: batches r holds in memory
          ddata,    \* ddata[r]: batches with a durable `Data` record at r
          prop,     \* prop[p][s]: p's working value in slot s
          reg,      \* reg[r][s]: the value r's register holds for s (durable)
          decided,  \* decided[s]: the value s decided
          known,    \* known[r]: slots r learned decided (its durable `Decided` records)
          floor,    \* floor[r]: slots below it are in r's snapshot
          down,     \* replicas stopped for good
          crashes   \* crashes so far

vars == <<created, data, ddata, prop, reg, decided, known, floor, down, crashes>>

Live(r) == r \notin down

TypeOK ==
    /\ created \subseteq Batches
    /\ data \in [Replicas -> SUBSET Batches]
    /\ ddata \in [Replicas -> SUBSET Batches]
    /\ prop \in [Proposers -> [Slots -> SUBSET Batches]]
    /\ reg \in [Replicas -> [Slots -> SUBSET Batches]]
    /\ decided \in [Slots -> SUBSET Batches]
    /\ known \in [Replicas -> SUBSET Slots]
    /\ floor \in [Replicas -> 0..NS]
    /\ down \subseteq Replicas
    /\ crashes \in 0..MaxCrash

Init ==
    /\ created = {} /\ down = {} /\ crashes = 0
    /\ data = [r \in Replicas |-> {}] /\ ddata = [r \in Replicas |-> {}]
    /\ prop = [p \in Proposers |-> [s \in Slots |-> {}]]
    /\ reg = [r \in Replicas |-> [s \in Slots |-> {}]]
    /\ decided = [s \in Slots |-> {}]
    /\ known = [r \in Replicas |-> {}] /\ floor = [r \in Replicas |-> 0]

\* The origin cuts batch b (`forward_pending`) and sends `Data` to every member: it reaches
\* the live ones in `to` and is lost to the others.
Create(b, to) ==
    LET o == OriginOf[b] IN
    /\ b \notin created /\ Live(o)
    /\ created' = created \cup {b}
    /\ data' = [r \in Replicas |->
                  IF r = o \/ (r \in to /\ Live(r)) THEN data[r] \cup {b} ELSE data[r]]
    /\ UNCHANGED <<ddata, prop, reg, decided, known, floor, down, crashes>>

\* Why r may need b: a request names it (a parked request), a recorder reports it (a value
\* a proposer adopts), or a decided slot r learned does.
Wants(r, b) ==
    \/ \E p \in Proposers, s \in Slots : b \in prop[p][s]
    \/ r \in Proposers /\ \E x \in Replicas, s \in Slots : b \in reg[x][s]
    \/ \E s \in known[r] : s >= floor[r] /\ b \in decided[s]

\* r fetches b from q, which holds it: for a parked request (from its requester), a value to
\* adopt (from the recorder that reported it) or a decided slot. Without fairness it may
\* never succeed.
Fetch(r, q, b) ==
    /\ Live(r) /\ Live(q) /\ r # q
    /\ b \in data[q] \ data[r]
    /\ Wants(r, b)
    /\ data' = [data EXCEPT ![r] = @ \cup {b}]
    /\ UNCHANGED <<created, ddata, prop, reg, decided, known, floor, down, crashes>>

\* r, blocked on decided slot s, asks q, which compacted s: q answers `Compacted` (the fetch
\* names the slot, DEV-3) and r installs q's snapshot (`SnapshotNeeded`, guarded by the
\* applied prefix, not the decided one).
Compacted(r, q, s) ==
    /\ Live(r) /\ Live(q) /\ r # q
    /\ s \in known[r] /\ s >= floor[r] /\ ~(decided[s] \subseteq data[r])
    /\ s < floor[q]
    /\ floor' = [floor EXCEPT ![r] = floor[q]]
    /\ reg' = [reg EXCEPT ![r] = [t \in Slots |-> IF t < floor[q] THEN {} ELSE reg[r][t]]]
    /\ UNCHANGED <<created, data, ddata, prop, decided, known, down, crashes>>

\* Rule P: p proposes in s only a value whose content it holds (fetching first otherwise).
Propose(p, s, v) ==
    /\ Live(p) /\ decided[s] = {} /\ s >= floor[p] /\ prop[p][s] = {}
    /\ v \subseteq data[p]
    /\ prop' = [prop EXCEPT ![p][s] = v]
    /\ UNCHANGED <<created, data, ddata, reg, decided, known, floor, down, crashes>>

\* `on_request`: r records p's working value for s (`would_change`: its register is empty)
\* only while it holds all its content (rule R), and writes that content's `Data` records
\* ahead of the `Recorder` record in the same persist: records are durable in order, and
\* the reply waits for them.
Record(r, p, s) ==
    LET v == prop[p][s] IN
    /\ Live(r) /\ v # {}
    /\ s >= floor[r] /\ s \notin known[r] /\ reg[r][s] = {}
    /\ v \subseteq data[r]
    /\ ddata' = [ddata EXCEPT ![r] = @ \cup (v \cap data[r])]
    /\ reg' = [reg EXCEPT ![r][s] = v]
    /\ UNCHANGED <<created, data, prop, decided, known, floor, down, crashes>>

\* A majority of registers hold v for s.
Decide(s, v) ==
    /\ decided[s] = {} /\ v # {}
    /\ Maj({q \in Replicas : reg[q][s] = v})
    /\ decided' = [decided EXCEPT ![s] = v]
    /\ UNCHANGED <<created, data, ddata, prop, reg, known, floor, down, crashes>>

\* `learn`, with its `Decided` record durable, so the register goes too
\* (`Effect::DecidedDurable`); a proposer in s is done with it.
Learn(r, s) ==
    /\ Live(r) /\ decided[s] # {} /\ s \notin known[r] /\ s >= floor[r]
    /\ known' = [known EXCEPT ![r] = @ \cup {s}]
    /\ reg' = [reg EXCEPT ![r][s] = {}]
    /\ prop' = IF r \in Proposers THEN [prop EXCEPT ![r][s] = {}] ELSE prop
    /\ UNCHANGED <<created, data, ddata, decided, floor, down, crashes>>

\* What the store must keep at r: content a register, a decided slot at or above the floor,
\* a working value or (at its origin, until committed) a batch needs.
Pinned(r, b) ==
    \/ \E t \in Slots : b \in reg[r][t]
    \/ \E t \in known[r] : t >= floor[r] /\ b \in decided[t]
    \/ r \in Proposers /\ \E t \in Slots : b \in prop[r][t]
    \/ OriginOf[b] = r /\ ~\E t \in known[r] : b \in decided[t]

Evict(r, b) ==
    /\ Live(r) /\ b \in data[r] /\ ~Pinned(r, b)
    /\ data' = [data EXCEPT ![r] = @ \ {b}]
    /\ ddata' = [ddata EXCEPT ![r] = @ \ {b}]
    /\ UNCHANGED <<created, prop, reg, decided, known, floor, down, crashes>>

\* r applied every slot below k (each learned, its content held) and takes a durable
\* snapshot there; registers below it go (`raise_floor`).
Compact(r, k) ==
    /\ Live(r) /\ k \in floor[r]+1..NS
    /\ \A t \in floor[r]..k-1 : t \in known[r] /\ decided[t] \subseteq data[r]
    /\ floor' = [floor EXCEPT ![r] = k]
    /\ reg' = [reg EXCEPT ![r] = [t \in Slots |-> IF t < k THEN {} ELSE reg[r][t]]]
    /\ UNCHANGED <<created, data, ddata, prop, decided, known, down, crashes>>

\* A crash and restart: memory is rebuilt from the disk.
Crash(r) ==
    /\ Live(r) /\ crashes < MaxCrash
    /\ crashes' = crashes + 1
    /\ data' = [data EXCEPT ![r] = ddata[r]]
    /\ prop' = IF r \in Proposers THEN [prop EXCEPT ![r] = [s \in Slots |-> {}]] ELSE prop
    /\ UNCHANGED <<created, ddata, reg, decided, known, floor, down>>

\* r stops for good; a majority keeps running.
Stop(r) ==
    /\ Live(r) /\ Cardinality(down) < MaxStop
    /\ 2 * Cardinality(down \cup {r}) < Cardinality(Replicas)
    /\ down' = down \cup {r}
    /\ UNCHANGED <<created, data, ddata, prop, reg, decided, known, floor, crashes>>

Next ==
    \/ \E b \in Batches, to \in SUBSET Replicas : Create(b, to)
    \/ \E r, q \in Replicas : \E b \in Batches : Fetch(r, q, b)
    \/ \E r, q \in Replicas : \E s \in Slots : Compacted(r, q, s)
    \/ \E p \in Proposers, s \in Slots : \E v \in Vals[s] : Propose(p, s, v)
    \/ \E r \in Replicas, p \in Proposers, s \in Slots : Record(r, p, s)
    \/ \E s \in Slots : \E v \in Vals[s] : Decide(s, v)
    \/ \E r \in Replicas, s \in Slots : Learn(r, s)
    \/ \E r \in Replicas, b \in Batches : Evict(r, b)
    \/ \E r \in Replicas, k \in Slots \cup {NS} : Compact(r, k)
    \/ \E r \in Replicas : Crash(r) \/ Stop(r)

\* r fetches a batch of decided slot s it lacks from member q: q sends it, or answers
\* `Compacted`.
Serve(r, q, s, b) ==
    /\ s \in known[r] /\ s >= floor[r] /\ b \in decided[s]
    /\ Fetch(r, q, b) \/ Compacted(r, q, s)

Fairness ==
    /\ \A r \in Replicas, s \in Slots : WF_vars(Learn(r, s))
    /\ \A r, q \in Replicas : \A s \in Slots, b \in Batches : WF_vars(Serve(r, q, s, b))

Spec == Init /\ [][Next]_vars /\ Fairness

\* q keeps decided slot s: its content is durable there, or s is in q's snapshot.
Covered(q, s) == s < floor[q] \/ decided[s] \subseteq ddata[q]

DecidedContentDurable ==
    \A s \in Slots : decided[s] # {} =>
        \E Q \in SUBSET Replicas : Maj(Q) /\ \A q \in Q : Covered(q, s)

Holds(r, s) == s < floor[r] \/ decided[s] \subseteq data[r]

ContentSpreads ==
    \A s \in Slots : decided[s] # {} ~> \A r \in Replicas \ down : Holds(r, s)
=============================================================================

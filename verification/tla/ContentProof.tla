---------------------------- MODULE ContentProof ----------------------------
(***************************************************************************)
(* TLAPS proof of Content.tla's DecidedContentDurable, for any number of  *)
(* replicas, batches, values, slots, proposers, crashes and stopped       *)
(* replicas.                                                               *)
(*                                                                         *)
(* Rule R makes every register's content durable at its recorder          *)
(* (RegDurable: G6a). A slot decides on a majority of registers holding   *)
(* its value, so that majority holds the content durably at that point.   *)
(* Each of them keeps it (Keeps): the register stays until the decision   *)
(* is durable there, which then pins the content until compaction, and a  *)
(* compacted slot is below the floor. Eviction never drops pinned content *)
(* and a crash keeps the disk, so the same majority witnesses             *)
(* DecidedContentDurable from then on.                                    *)
(***************************************************************************)
EXTENDS Content, TLAPS

ASSUME CAssumptions ==
    /\ NS \in Nat
    /\ Proposers \subseteq Replicas
    /\ OriginOf \in [Batches -> Replicas]
    /\ Vals \in [Slots -> SUBSET SUBSET Batches]
    /\ MaxCrash \in Nat
    /\ MaxStop \in Nat

\* G6a: every register's content is durable at its recorder.
RegDurable == \A r \in Replicas, s \in Slots : reg[r][s] \subseteq ddata[r]

\* q keeps decided slot s: below the floor, or its content durable and pinned by the
\* register (still holding the decided value) or by the durable decision.
Keeps(q, s) ==
    \/ s < floor[q]
    \/ /\ decided[s] \subseteq ddata[q]
       /\ reg[q][s] = decided[s] \/ s \in known[q]

Custody ==
    \A s \in Slots : decided[s] # {} =>
        \E Q \in SUBSET Replicas : Maj(Q) /\ \A q \in Q : Keeps(q, s)

IndInv == TypeOK /\ RegDurable /\ Custody

-----------------------------------------------------------------------------

THEOREM InitInv == Init => IndInv
BY CAssumptions DEF Init, IndInv, TypeOK, RegDurable, Custody, Slots

\* A step that decides nothing and keeps every member keeping: the same quorums witness.
LEMMA CustodyStep ==
    ASSUME Custody, decided' = decided,
           \A q \in Replicas, s \in Slots : decided[s] # {} /\ Keeps(q, s) => Keeps(q, s)'
    PROVE  Custody'
<1> SUFFICES ASSUME NEW s \in Slots, decided[s] # {}
             PROVE  \E Q \in SUBSET Replicas : Maj(Q) /\ \A q \in Q : Keeps(q, s)'
  BY DEF Custody
<1>1. PICK Q \in SUBSET Replicas : Maj(Q) /\ \A q \in Q : Keeps(q, s) BY DEF Custody
<1> QED BY <1>1

\* The core lemma: rule R, then a decision, leaves the content durable at the deciding
\* majority.
LEMMA DecideKeeps ==
    ASSUME IndInv, NEW s \in Slots, NEW v \in Vals[s], Decide(s, v)
    PROVE  IndInv'
<1> DEFINE Q == {q \in Replicas : reg[q][s] = v}
<1>1. /\ \A q \in Q : v \subseteq ddata[q]
      /\ Maj(Q) /\ decided[s] = {} /\ v # {}
      /\ decided' = [decided EXCEPT ![s] = v]
  BY DEF Decide, IndInv, RegDurable
<1>2. TypeOK' BY <1>1, CAssumptions DEF Decide, IndInv, TypeOK
<1>3. RegDurable' BY DEF Decide, IndInv, RegDurable
<1>4. Custody'
  <2> SUFFICES ASSUME NEW t \in Slots, decided'[t] # {}
               PROVE  \E P \in SUBSET Replicas : Maj(P) /\ \A q \in P : Keeps(q, t)'
    BY DEF Custody
  <2>1. CASE t = s
    <3>1. \A q \in Q : Keeps(q, s)' BY <1>1 DEF Decide, IndInv, TypeOK, Keeps
    <3> QED BY <1>1, <2>1, <3>1
  <2>2. CASE t # s
    <3>1. decided'[t] = decided[t] BY <1>1, <2>2 DEF IndInv, TypeOK
    <3>2. PICK P \in SUBSET Replicas : Maj(P) /\ \A q \in P : Keeps(q, t)
      BY <3>1, <2>2 DEF IndInv, Custody
    <3>3. \A q \in P : Keeps(q, t)' BY <3>1, <3>2 DEF Decide, Keeps
    <3> QED BY <3>2, <3>3
  <2> QED BY <2>1, <2>2
<1> QED BY <1>2, <1>3, <1>4 DEF IndInv

LEMMA RecordKeeps ==
    ASSUME IndInv, NEW r \in Replicas, NEW p \in Proposers, NEW s \in Slots, Record(r, p, s)
    PROVE  IndInv'
<1> DEFINE v == prop[p][s]
<1>1. /\ reg' = [reg EXCEPT ![r][s] = v]
      /\ ddata' = [ddata EXCEPT ![r] = @ \cup (v \cap data[r])]
      /\ v \subseteq data[r]
      /\ s >= floor[r] /\ s \notin known[r] /\ reg[r][s] = {}
  BY DEF Record
\* The content's `Data` records land with the register: rule R, then durable in order.
<1>2. /\ v \subseteq ddata'[r]
      /\ \A x \in Replicas : ddata[x] \subseteq ddata'[x]
  BY <1>1 DEF IndInv, TypeOK
<1>3. TypeOK' BY <1>1, CAssumptions DEF Record, IndInv, TypeOK
<1>4. RegDurable' BY <1>1, <1>2 DEF Record, IndInv, TypeOK, RegDurable
<1>5. \A q \in Replicas, t \in Slots : decided[t] # {} /\ Keeps(q, t) => Keeps(q, t)'
  <2> SUFFICES ASSUME NEW q \in Replicas, NEW t \in Slots, decided[t] # {}, Keeps(q, t)
               PROVE  Keeps(q, t)'
    OBVIOUS
  \* An empty register, not learned, at or above the floor: r did not keep s before.
  <2>1. CASE q = r /\ t = s BY <1>1, <2>1 DEF IndInv, TypeOK, Slots, Keeps
  <2>2. CASE ~(q = r /\ t = s)
    <3>1. reg'[q][t] = reg[q][t] BY <1>1, <2>2 DEF IndInv, TypeOK
    <3> QED BY <1>2, <3>1 DEF Record, Keeps
  <2> QED BY <2>1, <2>2
<1>6. Custody' BY <1>5, CustodyStep DEF Record, IndInv
<1> QED BY <1>3, <1>4, <1>6 DEF IndInv

LEMMA LearnKeeps ==
    ASSUME IndInv, NEW r \in Replicas, NEW s \in Slots, Learn(r, s)
    PROVE  IndInv'
<1>1. /\ reg' = [reg EXCEPT ![r][s] = {}]
      /\ known' = [known EXCEPT ![r] = @ \cup {s}]
  BY DEF Learn
<1>2. TypeOK' BY <1>1, CAssumptions DEF Learn, IndInv, TypeOK
<1>3. RegDurable' BY <1>1 DEF Learn, IndInv, TypeOK, RegDurable
<1>4. \A q \in Replicas, t \in Slots : decided[t] # {} /\ Keeps(q, t) => Keeps(q, t)'
  <2> SUFFICES ASSUME NEW q \in Replicas, NEW t \in Slots, decided[t] # {}, Keeps(q, t)
               PROVE  Keeps(q, t)'
    OBVIOUS
  <2>1. known[q] \subseteq known'[q] BY <1>1 DEF IndInv, TypeOK
  <2>2. CASE q = r /\ t = s BY <1>1, <2>2 DEF Learn, IndInv, TypeOK, Keeps
  <2>3. CASE ~(q = r /\ t = s)
    <3>1. reg'[q][t] = reg[q][t] BY <1>1, <2>3 DEF IndInv, TypeOK
    <3> QED BY <2>1, <3>1 DEF Learn, Keeps
  <2> QED BY <2>2, <2>3
<1>5. Custody' BY <1>4, CustodyStep DEF Learn, IndInv
<1> QED BY <1>2, <1>3, <1>5 DEF IndInv

LEMMA EvictKeeps ==
    ASSUME IndInv, NEW r \in Replicas, NEW b \in Batches, Evict(r, b)
    PROVE  IndInv'
<1>1. /\ \A t \in Slots : b \notin reg[r][t]
      /\ \A t \in known[r] : t >= floor[r] => b \notin decided[t]
  BY DEF Evict, Pinned
<1>2. ddata'[r] = ddata[r] \ {b} /\ \A x \in Replicas \ {r} : ddata'[x] = ddata[x]
  BY DEF Evict, IndInv, TypeOK
<1>3. TypeOK' BY DEF Evict, IndInv, TypeOK
<1>4. RegDurable' BY <1>1, <1>2 DEF Evict, IndInv, TypeOK, RegDurable
<1>5. \A q \in Replicas, t \in Slots : decided[t] # {} /\ Keeps(q, t) => Keeps(q, t)'
  <2> SUFFICES ASSUME NEW q \in Replicas, NEW t \in Slots, decided[t] # {}, Keeps(q, t)
               PROVE  Keeps(q, t)'
    OBVIOUS
  <2>1. CASE q # r BY <1>2, <2>1 DEF Evict, Keeps
  <2>2. CASE q = r /\ t < floor[r] BY <2>2 DEF Evict, Keeps
  <2>3. CASE q = r /\ ~(t < floor[r])
    <3>1. t >= floor[r] BY <2>3 DEF IndInv, TypeOK, Slots
    \* Pinned by the register or by the durable decision.
    <3>2. b \notin decided[t] BY <1>1, <2>3, <3>1 DEF Keeps
    <3> QED BY <1>2, <2>3, <3>2 DEF Evict, Keeps
  <2> QED BY <2>1, <2>2, <2>3
<1>6. Custody' BY <1>5, CustodyStep DEF Evict, IndInv
<1> QED BY <1>3, <1>4, <1>6 DEF IndInv

\* Raising r's floor past every slot it drops a register for.
LEMMA RaiseFloorKeeps ==
    ASSUME IndInv, NEW r \in Replicas, NEW k \in 0..NS, k > floor[r],
           floor' = [floor EXCEPT ![r] = k],
           reg' = [reg EXCEPT ![r] = [t \in Slots |-> IF t < k THEN {} ELSE reg[r][t]]],
           UNCHANGED <<created, data, ddata, prop, decided, known, down, crashes>>
    PROVE  IndInv'
<1>1. TypeOK' BY CAssumptions DEF IndInv, TypeOK
<1>2. RegDurable' BY DEF IndInv, TypeOK, RegDurable
<1>3. \A q \in Replicas, t \in Slots : decided[t] # {} /\ Keeps(q, t) => Keeps(q, t)'
  <2> SUFFICES ASSUME NEW q \in Replicas, NEW t \in Slots, decided[t] # {}, Keeps(q, t)
               PROVE  Keeps(q, t)'
    OBVIOUS
  <2>1. CASE q # r BY <2>1 DEF IndInv, TypeOK, Keeps
  <2>2. CASE q = r /\ t < k BY <2>2, CAssumptions DEF IndInv, TypeOK, Keeps, Slots
  <2>3. CASE q = r /\ ~(t < k)
    <3>1. reg'[r][t] = reg[r][t] /\ ~(t < floor[r])
      BY <2>3, CAssumptions DEF IndInv, TypeOK, Slots
    <3> QED BY <2>3, <3>1, CAssumptions DEF IndInv, TypeOK, Keeps, Slots
  <2> QED BY <2>1, <2>2, <2>3
<1>4. Custody' BY <1>3, CustodyStep DEF IndInv
<1> QED BY <1>1, <1>2, <1>4 DEF IndInv

\* Steps that leave the disk (ddata, reg, decided, known, floor) alone.
LEMMA DiskKeeps ==
    ASSUME IndInv, TypeOK', UNCHANGED <<ddata, reg, decided, known, floor>>
    PROVE  IndInv'
<1>1. RegDurable' BY DEF IndInv, RegDurable
<1>2. \A q \in Replicas, t \in Slots : decided[t] # {} /\ Keeps(q, t) => Keeps(q, t)'
  BY DEF Keeps
<1>3. Custody' BY <1>2, CustodyStep DEF IndInv
<1> QED BY <1>1, <1>3 DEF IndInv

THEOREM NextInv == IndInv /\ [Next]_vars => IndInv'
<1> SUFFICES ASSUME IndInv, [Next]_vars PROVE IndInv' OBVIOUS
<1>0. TypeOK BY DEF IndInv
<1>1. ASSUME NEW b \in Batches, NEW to \in SUBSET Replicas, Create(b, to) PROVE IndInv'
  <2>1. TypeOK' BY <1>0, <1>1, CAssumptions DEF Create, TypeOK
  <2> QED BY <1>1, <2>1, DiskKeeps DEF Create
<1>2. ASSUME NEW r \in Replicas, NEW q \in Replicas, NEW b \in Batches, Fetch(r, q, b)
      PROVE IndInv'
  <2>1. TypeOK' BY <1>0, <1>2 DEF Fetch, TypeOK
  <2> QED BY <1>2, <2>1, DiskKeeps DEF Fetch
<1>3. ASSUME NEW r \in Replicas, NEW q \in Replicas, NEW s \in Slots, Compacted(r, q, s)
      PROVE IndInv'
  <2>1. floor[q] \in 0..NS /\ floor[q] > floor[r]
    BY <1>0, <1>3, CAssumptions DEF Compacted, TypeOK, Slots
  <2> QED BY <1>3, <2>1, RaiseFloorKeeps DEF Compacted
<1>4. ASSUME NEW p \in Proposers, NEW s \in Slots, NEW v \in Vals[s], Propose(p, s, v)
      PROVE IndInv'
  <2>1. TypeOK' BY <1>0, <1>4, CAssumptions DEF Propose, TypeOK
  <2> QED BY <1>4, <2>1, DiskKeeps DEF Propose
<1>5. ASSUME NEW r \in Replicas, NEW p \in Proposers, NEW s \in Slots, Record(r, p, s)
      PROVE IndInv'
  BY <1>5, RecordKeeps
<1>6. ASSUME NEW s \in Slots, NEW v \in Vals[s], Decide(s, v) PROVE IndInv'
  BY <1>6, DecideKeeps
<1>7. ASSUME NEW r \in Replicas, NEW s \in Slots, Learn(r, s) PROVE IndInv'
  BY <1>7, LearnKeeps
<1>8. ASSUME NEW r \in Replicas, NEW b \in Batches, Evict(r, b) PROVE IndInv'
  BY <1>8, EvictKeeps
<1>9. ASSUME NEW r \in Replicas, NEW k \in Slots \cup {NS}, Compact(r, k) PROVE IndInv'
  <2>1. k \in 0..NS /\ k > floor[r] BY <1>0, <1>9, CAssumptions DEF Compact, TypeOK, Slots
  <2> QED BY <1>9, <2>1, RaiseFloorKeeps DEF Compact
<1>10. ASSUME NEW r \in Replicas, Crash(r) PROVE IndInv'
  <2>1. TypeOK' BY <1>0, <1>10, CAssumptions DEF Crash, TypeOK
  <2> QED BY <1>10, <2>1, DiskKeeps DEF Crash
<1>11. ASSUME NEW r \in Replicas, Stop(r) PROVE IndInv'
  <2>1. TypeOK' BY <1>0, <1>11 DEF Stop, TypeOK
  <2> QED BY <1>11, <2>1, DiskKeeps DEF Stop
<1>12. CASE UNCHANGED vars
  <2>1. TypeOK' BY <1>0, <1>12 DEF vars, TypeOK
  <2> QED BY <1>12, <2>1, DiskKeeps DEF vars
<1> QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12
        DEF Next

THEOREM IndDurable == IndInv => DecidedContentDurable
BY DEF IndInv, Custody, DecidedContentDurable, Keeps, Covered

THEOREM ContentSafe == Spec => []DecidedContentDurable
<1>1. Init => IndInv BY InitInv
<1>2. IndInv /\ [Next]_vars => IndInv' BY NextInv
<1>3. IndInv => DecidedContentDurable BY IndDurable
<1> QED BY <1>1, <1>2, <1>3, PTL DEF Spec
=============================================================================

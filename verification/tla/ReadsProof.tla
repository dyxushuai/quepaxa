---------------------------- MODULE ReadsProof ----------------------------
(***************************************************************************)
(* TLAPS proof of Reads.tla's ReadSafe with fixed membership (no change), *)
(* for any number of replicas, slots and reads: a ready read reflects     *)
(* every slot decided before it was requested.                             *)
(*                                                                         *)
(* A decided slot was recorded durably by a majority of the members, and  *)
(* stays covered there (DCov): a register goes only once the decision is  *)
(* durable, and a compacted slot is below the floor. A covered slot is    *)
(* below what its recorder answers. So the majority answering a read meets *)
(* that one in a recorder whose answer exceeds the slot, and the read     *)
(* waits until its replica applied past the largest answer.               *)
(*                                                                         *)
(* Membership changes are checked by TLC (run.sh), not proved here.       *)
(***************************************************************************)
EXTENDS Reads, MembershipProof, FiniteSetTheorems

ASSUME FixedAssumptions ==
    /\ Changes = {}
    /\ IsFiniteSet(Replicas)
    /\ InitMembers \subseteq Replicas /\ InitMembers # {}
    /\ 0 \notin Replicas

-----------------------------------------------------------------------------
(* Without changes, every slot's members are the configured ones *)

NoChange(val) == \A t \in Slots : val[t].k # "change"

LEMMA FoldedFixed ==
    ASSUME NEW val, NoChange(val)
    PROVE  \A n \in 0..N : Folded(val)[n] = Start
<1> DEFINE P(n) == n <= N => Folded(val)[n] = Start
<1>1. P(0) BY FoldedDef
<1>2. \A n \in Nat : P(n) => P(n + 1)
  <2> SUFFICES ASSUME NEW n \in Nat, P(n), n + 1 <= N PROVE Folded(val)[n + 1] = Start OBVIOUS
  <2>1. Folded(val)[n] = Start /\ n \in Slots /\ val[n].k # "change"
    BY MAssumptions DEF NoChange, Slots, P
  <2>2. Folded(val)[n + 1] = FoldStep(Start, n, val) BY <2>1, FoldedDef
  <2> QED BY <2>1, <2>2 DEF FoldStep, Start
<1> HIDE DEF P
<1>3. \A n \in Nat : P(n) BY <1>1, <1>2, NatInduction
<1> QED BY <1>3, MAssumptions DEF P

LEMMA MembersFixed ==
    ASSUME NEW val, NoChange(val), NEW s \in Slots
    PROVE  MembersAt(s, val) = InitMembers
<1>1. s \in 0..N BY MAssumptions DEF Slots
<1>2. Folded(val)[s] = Start BY <1>1, FoldedFixed
<1> QED BY <1>2, MAssumptions DEF MembersAt, Start, Slots

-----------------------------------------------------------------------------
(* Arithmetic and sets *)

LEMMA MaxProps ==
    ASSUME NEW n \in Nat, NEW S \in SUBSET (0..n), S # {}
    PROVE  Max(S) \in S /\ \A x \in S : x <= Max(S)
<1> DEFINE P(m) == \A T \in SUBSET (0..m) : T # {} => \E y \in T : \A x \in T : x <= y
<1>1. P(0) OBVIOUS
<1>2. \A m \in Nat : P(m) => P(m + 1)
  <2> SUFFICES ASSUME NEW m \in Nat, P(m), NEW T \in SUBSET (0..(m + 1)), T # {}
               PROVE  \E y \in T : \A x \in T : x <= y
    OBVIOUS
  <2>1. CASE m + 1 \in T BY <2>1
  <2>2. CASE m + 1 \notin T
    <3>1. T \in SUBSET (0..m) BY <2>2
    <3> QED BY <3>1
  <2> QED BY <2>1, <2>2
<1> HIDE DEF P
<1>3. \A m \in Nat : P(m) BY <1>1, <1>2, NatInduction
<1>4. \E y \in S : \A x \in S : x <= y BY <1>3 DEF P
<1> QED BY <1>4 DEF Max

LEMMA QuorumsMeet ==
    ASSUME NEW E, IsFiniteSet(E), NEW Q \in SUBSET E, NEW A \in SUBSET E,
           2 * Cardinality(Q) > Cardinality(E), 2 * Cardinality(A) > Cardinality(E)
    PROVE  Q \cap A # {}
<1> SUFFICES ASSUME Q \cap A = {} PROVE FALSE OBVIOUS
<1>1. IsFiniteSet(Q) /\ IsFiniteSet(A) BY FS_Subset
<1>2. Cardinality(Q \cup A) = Cardinality(Q) + Cardinality(A) - Cardinality(Q \cap A)
  BY <1>1, FS_Union
<1>3. Cardinality(Q \cap A) = 0 BY FS_EmptySet
<1>4. Cardinality(Q \cup A) <= Cardinality(E) BY <1>1, FS_Subset
<1>5. Cardinality(Q) \in Nat /\ Cardinality(A) \in Nat /\ Cardinality(E) \in Nat
  BY <1>1, FS_CardinalityType
<1> QED BY <1>2, <1>3, <1>4, <1>5

-----------------------------------------------------------------------------
(* The invariant *)

\* Slot s is covered by what q holds durably.
DCov(q, s) == s < floor[q] \/ s \in reg[q] \/ s \in dlog[q]

ReadState == [st : {"idle", "open", "ready"}, at : Replicas \cup {0}, before : SUBSET Slots,
              who : SUBSET Replicas, k : Nat, applied : Nat]

RInvTypes ==
    /\ decided \in [Slots -> {Write, None}]
    /\ log \in [Replicas -> SUBSET Slots]
    /\ dlog \in [Replicas -> SUBSET Slots]
    /\ vreg \in [Replicas -> SUBSET Slots]
    /\ reg \in [Replicas -> SUBSET Slots]
    /\ floor \in [Replicas -> 0..N]
    /\ rd \in [ReadIds -> ReadState]
    /\ retired = {}
    /\ \A r \in Replicas : dlog[r] \subseteq log[r] /\ log[r] \subseteq Decided

\* Every decided slot is durably covered at a majority of the members.
Covered(s) == \E Q \in Maj(InitMembers) : \A q \in Q : DCov(q, s)

\* An open read: every slot it must reflect is covered at a majority whose answers so far
\* all exceed it.
OpenOK(i) ==
    /\ rd[i].at \in Replicas
    /\ \A t \in rd[i].before :
           /\ decided[t] # None
           /\ \E Q \in Maj(InitMembers) :
                  /\ \A q \in Q : DCov(q, t)
                  /\ \A q \in Q \cap rd[i].who : t < rd[i].k

FInv ==
    /\ RInvTypes
    /\ \A s \in Slots : decided[s] # None => Covered(s)
    /\ \A i \in ReadIds : rd[i].st = "open" => OpenOK(i)
    /\ ReadSafe

-----------------------------------------------------------------------------
(* Consequences of the types *)

LEMMA KnownNoChange ==
    ASSUME RInvTypes, NEW r \in Replicas
    PROVE  NoChange(Known(r))
BY FixedAssumptions DEF RInvTypes, NoChange, Known, Write, None, Values

LEMMA PrefixKnownNoChange ==
    ASSUME RInvTypes, NEW r \in Replicas
    PROVE  NoChange(PrefixKnown(r))
BY FixedAssumptions DEF RInvTypes, NoChange, PrefixKnown, Write, None, Values

LEMMA CoveringFixed ==
    ASSUME RInvTypes, NEW r \in Replicas, NEW k \in Nat
    PROVE  Covering(r, k) = {InitMembers}
<1>1. NoChange(Known(r)) BY KnownNoChange
<1>2. \A t \in k..N-1 : t \in Slots => MembersAt(t, PrefixKnown(r)) = InitMembers
  BY PrefixKnownNoChange, MembersFixed
<1>3. Prefix(log[r]) \in 0..N BY PrefixProps DEF RInvTypes
<1>4. Latest(r) = Start BY <1>1, <1>3, FoldedFixed DEF Latest
<1>5. \A t \in k..N-1 : t \in Slots BY MAssumptions DEF Slots
<1> QED BY <1>2, <1>4, <1>5 DEF Covering, Start

\* What q answers: the largest of a nonempty set of 0..N.
LEMMA MarkProps ==
    ASSUME RInvTypes, NEW q \in Replicas
    PROVE  LET held == vreg[q] \cup reg[q] \cup {t \in log[q] : t >= floor[q]}
               S == {floor[q]} \cup {t + 1 : t \in held}
           IN  /\ Mark(q) \in S /\ \A x \in S : x <= Mark(q)
               /\ Mark(q) \in 0..N
<1> DEFINE held == vreg[q] \cup reg[q] \cup {t \in log[q] : t >= floor[q]}
           S == {floor[q]} \cup {t + 1 : t \in held}
<1>1. held \subseteq Slots /\ floor[q] \in 0..N BY DEF RInvTypes
<1>2. S \in SUBSET (0..N) /\ S # {} BY <1>1, MAssumptions DEF Slots
<1>3. Max(S) \in S /\ \A x \in S : x <= Max(S) BY <1>2, MaxProps, MAssumptions
<1> QED BY <1>2, <1>3 DEF Mark

\* What q answers exceeds every slot it covers.
LEMMA CovMark ==
    ASSUME RInvTypes, NEW q \in Replicas, NEW s \in Slots, DCov(q, s)
    PROVE  s < Mark(q)
<1> DEFINE held == vreg[q] \cup reg[q] \cup {t \in log[q] : t >= floor[q]}
           S == {floor[q]} \cup {t + 1 : t \in held}
<1>1. Mark(q) \in 0..N /\ \A x \in S : x <= Mark(q) BY MarkProps
<1>2. s \in Nat /\ floor[q] \in Nat BY MAssumptions DEF Slots, RInvTypes
<1>3. CASE s < floor[q]
  <2>1. floor[q] \in S OBVIOUS
  <2> QED BY <1>1, <1>2, <1>3, <2>1
<1>4. CASE s \in held
  <2>1. s + 1 \in S BY <1>4
  <2> QED BY <1>1, <1>2, <1>4, <2>1
<1>5. dlog[q] \subseteq log[q] BY DEF RInvTypes
<1> QED BY <1>2, <1>3, <1>4, <1>5 DEF DCov

LEMMA MarkNat ==
    ASSUME RInvTypes, NEW q \in Replicas
    PROVE  Mark(q) \in 0..N
BY MarkProps

-----------------------------------------------------------------------------
(* FInv is inductive *)

LEMMA InitFInv == RInit => FInv
<1> SUFFICES ASSUME RInit PROVE FInv OBVIOUS
<1>1. RInvTypes BY FixedAssumptions, MAssumptions DEF RInit, Init, RInvTypes, Idle, ReadState, Decided
<1> QED BY <1>1 DEF RInit, Init, FInv, Idle, ReadSafe, None

\* Durable coverage that every replica keeps.
DCovKeep == \A q \in Replicas, u \in Slots : DCov(q, u) => DCov(q, u)'

LEMMA DCovGrow ==
    ASSUME \A q \in Replicas : floor'[q] = floor[q] /\ reg[q] \subseteq reg'[q] /\ dlog[q] \subseteq dlog'[q]
    PROVE  DCovKeep
BY DEF DCovKeep, DCov

LEMMA CoveredKept ==
    ASSUME DCovKeep, NEW s \in Slots, Covered(s)
    PROVE  Covered(s)'
<1>1. PICK Q \in Maj(InitMembers) : \A q \in Q : DCov(q, s) BY DEF Covered
<1>2. Q \subseteq Replicas BY FixedAssumptions DEF Maj
<1> QED BY <1>1, <1>2 DEF Covered, DCovKeep

LEMMA OpenKept ==
    ASSUME RInvTypes, DCovKeep, NEW i \in ReadIds, OpenOK(i), rd'[i] = rd[i],
           \A t \in Slots : decided[t] # None => decided'[t] # None
    PROVE  OpenOK(i)'
<1>1. rd[i].before \subseteq Slots BY DEF RInvTypes, ReadState
<1>2. \A t \in rd[i].before :
          \E Q \in Maj(InitMembers) :
              /\ \A q \in Q : DCov(q, t)'
              /\ \A q \in Q \cap rd[i].who : t < rd[i].k
  <2> SUFFICES ASSUME NEW t \in rd[i].before
               PROVE  \E Q \in Maj(InitMembers) :
                          /\ \A q \in Q : DCov(q, t)'
                          /\ \A q \in Q \cap rd[i].who : t < rd[i].k
    OBVIOUS
  <2>1. PICK Q \in Maj(InitMembers) :
            /\ \A q \in Q : DCov(q, t)
            /\ \A q \in Q \cap rd[i].who : t < rd[i].k
    BY DEF OpenOK
  <2>2. Q \subseteq Replicas BY FixedAssumptions DEF Maj
  <2> QED BY <2>1, <2>2, <1>1 DEF DCovKeep
<1> QED BY <1>1, <1>2 DEF OpenOK

\* A step that leaves decisions and reads alone and keeps durable coverage keeps FInv.
LEMMA ReadsKept ==
    ASSUME FInv, RInvTypes', DCovKeep, rd' = rd, decided' = decided
    PROVE  FInv'
<1>1. RInvTypes BY DEF FInv
<1>2. \A s \in Slots : decided'[s] # None => Covered(s)' BY CoveredKept DEF FInv
<1>3. \A i \in ReadIds : rd'[i].st = "open" => OpenOK(i)' BY <1>1, OpenKept DEF FInv
<1> QED BY <1>2, <1>3 DEF FInv, ReadSafe

\* Updates of one read leave the others as they were.
LEMMA OthersKept ==
    ASSUME FInv, RInvTypes', DCovKeep, decided' = decided, NEW i \in ReadIds,
           \A j \in ReadIds \ {i} : rd'[j] = rd[j]
    PROVE  \A j \in ReadIds \ {i} : (rd'[j].st = "open" => OpenOK(j)') /\ (rd'[j].st = "ready" =>
               \A t \in rd'[j].before : t < rd'[j].applied)
BY OpenKept DEF FInv, ReadSafe

LEMMA FInvNext == FInv /\ [RNext]_allvars => FInv'
<1> SUFFICES ASSUME FInv, [RNext]_allvars PROVE FInv' OBVIOUS
<1>t. RInvTypes BY DEF FInv
<1>0. \A v \in Values : v = Write BY FixedAssumptions DEF Values
<1>1. ASSUME NEW r \in Replicas, NEW s \in Slots, NEW v \in Values, RDecide(r, s, v) PROVE FInv'
  <2>1. PICK Q \in Maj(MembersAt(s, Known(r))) :
            /\ Decide(r, s, v)
            /\ reg' = [x \in Replicas |-> IF x \in Q THEN reg[x] \cup {s} ELSE reg[x]]
            /\ vreg' = [x \in Replicas |-> IF x \in Q THEN vreg[x] \ {s} ELSE vreg[x]]
            /\ UNCHANGED <<floor, rd>>
    BY <1>1 DEF RDecide
  <2>2. MembersAt(s, Known(r)) = InitMembers BY <1>t, KnownNoChange, MembersFixed
  <2>3. decided' = [decided EXCEPT ![s] = v] /\ log' = [log EXCEPT ![r] = @ \cup {s}]
        /\ dlog' = dlog /\ retired' = retired /\ decided[s] = None
    BY <2>1 DEF Decide, CanDrive
  <2>4. v = Write BY <1>0
  <2>q. Q \subseteq Replicas BY <2>2, FixedAssumptions DEF Maj
  <2>5. RInvTypes' BY <1>t, <2>1, <2>3, <2>4, <2>q DEF RInvTypes, Decided, None, Write
  <2>6. DCovKeep BY <2>1, <2>3, <2>q, DCovGrow
  <2>7. \A q \in Q : DCov(q, s)' BY <2>1, <2>q DEF DCov
  <2>8. \A u \in Slots : decided'[u] # None => Covered(u)'
    <3> SUFFICES ASSUME NEW u \in Slots, decided'[u] # None PROVE Covered(u)' OBVIOUS
    <3>1. CASE u = s BY <2>2, <2>7, <3>1 DEF Covered
    <3>2. CASE u # s
      <4>1. decided[u] # None BY <2>3, <3>2, <1>t DEF RInvTypes
      <4>2. Covered(u) BY <4>1 DEF FInv
      <4> QED BY <4>2, <2>6, CoveredKept
    <3> QED BY <3>1, <3>2
  <2>9. \A t \in Slots : decided[t] # None => decided'[t] # None BY <2>3, <1>t DEF RInvTypes
  <2>10. \A i \in ReadIds : rd'[i].st = "open" => OpenOK(i)'
    BY <2>1, <2>6, <2>9, <1>t, OpenKept DEF FInv
  <2> QED BY <2>1, <2>5, <2>8, <2>10 DEF FInv, ReadSafe
<1>2. ASSUME NEW r \in Replicas, NEW s \in Slots, Learn(r, s) /\ UNCHANGED rvars PROVE FInv'
  <2>1. RInvTypes' BY <1>2, <1>t DEF Learn, RInvTypes, rvars, Decided
  <2>2. DCovKeep BY <1>2, DCovGrow DEF Learn, rvars
  <2> QED BY <1>2, <2>1, <2>2, ReadsKept DEF Learn, rvars
<1>3. ASSUME NEW r \in Replicas, NEW s \in Slots, Record(r, s) PROVE FInv'
  <2>1. RInvTypes' BY <1>3, <1>t DEF Record, RInvTypes, Decided
  <2>2. DCovKeep BY <1>3, DCovGrow DEF Record
  <2> QED BY <1>3, <2>1, <2>2, ReadsKept DEF Record
<1>4. ASSUME NEW r \in Replicas, NEW s \in Slots, DropRegister(r, s) PROVE FInv'
  <2>1. RInvTypes' BY <1>4, <1>t DEF DropRegister, RInvTypes, Decided
  <2>2. DCovKeep
    <3> SUFFICES ASSUME NEW q \in Replicas, NEW u \in Slots, DCov(q, u) PROVE DCov(q, u)'
      BY DEF DCovKeep
    <3>1. CASE q = r /\ u = s BY <1>4, <3>1 DEF DropRegister, DCov
    <3>2. CASE ~(q = r /\ u = s) BY <1>4, <3>2, <1>t DEF DropRegister, DCov, RInvTypes
    <3> QED BY <3>1, <3>2
  <2> QED BY <1>4, <2>1, <2>2, ReadsKept DEF DropRegister
<1>5. ASSUME NEW r \in Replicas, Persist(r) /\ UNCHANGED rvars PROVE FInv'
  <2>1. RInvTypes' BY <1>5, <1>t DEF Persist, RInvTypes, rvars, Decided
  <2>2. \A q \in Replicas : dlog[q] \subseteq dlog'[q] BY <1>5, <1>t DEF Persist, RInvTypes
  <2>3. DCovKeep BY <1>5, <2>2, DCovGrow DEF Persist, rvars
  <2> QED BY <1>5, <2>1, <2>3, ReadsKept DEF Persist, rvars
<1>6. ASSUME NEW r \in Replicas, RCrash(r) PROVE FInv'
  <2>1. RInvTypes' BY <1>6, <1>t DEF RCrash, RInvTypes, Decided
  <2>2. DCovKeep BY <1>6, DCovGrow DEF RCrash
  <2> QED BY <1>6, <2>1, <2>2, ReadsKept DEF RCrash
<1>7. ASSUME NEW r \in Replicas, Retire(r) /\ UNCHANGED rvars PROVE FInv'
  <2>1. Latest(r).from = 0
    <3>1. Prefix(log[r]) \in 0..N BY <1>t, PrefixProps DEF RInvTypes
    <3> QED BY <1>t, <3>1, KnownNoChange, FoldedFixed DEF Latest, Start
  <2> QED BY <1>7, <2>1 DEF Retire
<1>8. ASSUME NEW r \in Replicas, PersistRegisters(r) PROVE FInv'
  <2>1. RInvTypes' BY <1>8, <1>t DEF PersistRegisters, RInvTypes, Decided
  <2>2. \A q \in Replicas : reg[q] \subseteq reg'[q] BY <1>8, <1>t DEF PersistRegisters, RInvTypes
  <2>3. DCovKeep BY <1>8, <2>2, DCovGrow DEF PersistRegisters
  <2> QED BY <1>8, <2>1, <2>3, ReadsKept DEF PersistRegisters
<1>9. ASSUME NEW r \in Replicas, Compact(r) PROVE FInv'
  <2>1. Prefix(dlog[r]) \in 0..N BY <1>t, PrefixProps DEF RInvTypes
  <2>2. RInvTypes' BY <1>9, <1>t, <2>1 DEF Compact, RInvTypes, Decided
  <2>3. DCovKeep
    <3> SUFFICES ASSUME NEW q \in Replicas, NEW u \in Slots, DCov(q, u) PROVE DCov(q, u)'
      BY DEF DCovKeep
    <3>1. CASE q # r BY <1>9, <3>1, <1>t DEF Compact, DCov, RInvTypes
    <3>2. CASE q = r
      <4>1. floor'[r] = Prefix(dlog[r]) /\ floor[r] < Prefix(dlog[r])
            /\ reg'[r] = {t \in reg[r] : t >= Prefix(dlog[r])} /\ dlog' = dlog
        BY <1>9, <1>t DEF Compact, RInvTypes
      <4>2. u \in Nat /\ floor[r] \in Nat BY <1>t, MAssumptions DEF Slots, RInvTypes
      <4> QED BY <4>1, <4>2, <2>1, <3>2 DEF DCov
    <3> QED BY <3>1, <3>2
  <2> QED BY <1>9, <2>2, <2>3, ReadsKept DEF Compact
<1>10. ASSUME NEW i \in ReadIds, NEW r \in Replicas, Request(i, r) PROVE FInv'
  <2> DEFINE new == [st |-> "open", at |-> r, before |-> {t \in Slots : decided[t] # None},
                     who |-> {}, k |-> 1, applied |-> 0]
  <2>1. rd' = [rd EXCEPT ![i] = new] /\ rd[i].st = "idle"
        /\ UNCHANGED <<decided, log, dlog, retired, vreg, reg, floor>>
    BY <1>10 DEF Request
  <2>2. rd'[i] = new /\ \A j \in ReadIds \ {i} : rd'[j] = rd[j] BY <2>1, <1>t DEF RInvTypes
  <2>3. RInvTypes' BY <2>1, <2>2, <1>t, FixedAssumptions DEF RInvTypes, ReadState, Decided
  <2>4. DCovKeep BY <2>1, DCovGrow
  <2>5. OpenOK(i)'
    <3>0. decided' = decided /\ \A x, t : DCov(x, t)' = DCov(x, t) BY <2>1 DEF DCov
    <3>1. \A t \in new.before : decided[t] # None /\ Covered(t) BY DEF FInv
    <3>2. \A t \in new.before :
              \E Q \in Maj(InitMembers) :
                  /\ \A x \in Q : DCov(x, t)'
                  /\ \A x \in Q \cap new.who : t < new.k
      BY <3>0, <3>1 DEF Covered
    <3> QED BY <2>2, <3>0, <3>1, <3>2 DEF OpenOK
  <2>6. \A j \in ReadIds \ {i} : (rd'[j].st = "open" => OpenOK(j)') /\ (rd'[j].st = "ready" =>
            \A t \in rd'[j].before : t < rd'[j].applied)
    BY <2>1, <2>2, <2>3, <2>4, OthersKept
  <2>7. \A s \in Slots : decided'[s] # None => Covered(s)' BY <2>1, <2>4, CoveredKept DEF FInv
  <2> QED BY <2>2, <2>3, <2>5, <2>6, <2>7 DEF FInv, ReadSafe
<1>11. ASSUME NEW i \in ReadIds, NEW q \in Replicas, Answer(i, q) PROVE FInv'
  <2> DEFINE k2 == Max({rd[i].k, Mark(q)})
  <2>1. rd' = [rd EXCEPT ![i].who = @ \cup {q}, ![i].k = k2] /\ rd[i].st = "open"
        /\ UNCHANGED <<decided, log, dlog, retired, vreg, reg, floor>>
    BY <1>11 DEF Answer
  <2>0. rd[i] \in ReadState BY <1>t DEF RInvTypes
  <2>2. /\ rd'[i] = [rd[i] EXCEPT !.who = rd[i].who \cup {q}, !.k = k2]
        /\ \A j \in ReadIds \ {i} : rd'[j] = rd[j]
    BY <2>1, <1>t DEF RInvTypes
  <2>3. Mark(q) \in 0..N /\ rd[i].k \in Nat BY <1>t, MarkNat, <2>0 DEF ReadState
  <2>4. k2 \in Nat /\ rd[i].k <= k2 /\ Mark(q) <= k2
    <3>1. {rd[i].k, Mark(q)} \in SUBSET (0..(rd[i].k + N)) /\ rd[i].k + N \in Nat BY <2>3, MAssumptions
    <3>2. k2 \in {rd[i].k, Mark(q)} /\ \A x \in {rd[i].k, Mark(q)} : x <= k2 BY <3>1, MaxProps
    <3> QED BY <3>2, <2>3
  <2>5. /\ rd'[i].st = "open" /\ rd'[i].at = rd[i].at /\ rd'[i].before = rd[i].before
        /\ rd'[i].who = rd[i].who \cup {q} /\ rd'[i].k = k2
    BY <2>0, <2>1, <2>2 DEF ReadState
  <2>6. RInvTypes'
    <3>1. rd'[i] \in ReadState BY <2>0, <2>2, <2>4, <1>t DEF ReadState, RInvTypes
    <3>2. rd' \in [ReadIds -> ReadState] BY <3>1, <2>1, <2>2, <1>t DEF RInvTypes
    <3> QED BY <3>2, <2>1, <1>t DEF RInvTypes, Decided
  <2>7. DCovKeep BY <2>1, DCovGrow
  <2>8. OpenOK(i)'
    <3>1. rd[i].at \in Replicas /\ rd[i].before \subseteq Slots BY <2>1, <2>0 DEF FInv, OpenOK, ReadState
    <3>2. \A t \in rd[i].before :
              \E Q \in Maj(InitMembers) :
                  /\ \A x \in Q : DCov(x, t)
                  /\ \A x \in Q \cap rd'[i].who : t < rd'[i].k
      <4> SUFFICES ASSUME NEW t \in rd[i].before
                   PROVE  \E Q \in Maj(InitMembers) :
                              /\ \A x \in Q : DCov(x, t)
                              /\ \A x \in Q \cap rd'[i].who : t < rd'[i].k
        OBVIOUS
      <4>1. PICK Q \in Maj(InitMembers) :
                /\ \A x \in Q : DCov(x, t)
                /\ \A x \in Q \cap rd[i].who : t < rd[i].k
        BY <2>1 DEF FInv, OpenOK
      <4>2. t \in Slots /\ t \in Nat BY <3>1, MAssumptions DEF Slots
      <4>3. q \in Q => t < Mark(q) BY <4>1, <4>2, <1>t, CovMark
      <4>4. \A x \in Q \cap rd'[i].who : t < rd'[i].k
        BY <2>3, <2>4, <2>5, <4>1, <4>2, <4>3
      <4> QED BY <4>1, <4>4
    <3>3. \A t \in rd[i].before : decided[t] # None BY <2>1 DEF FInv, OpenOK
    <3>0. decided' = decided /\ \A x, t : DCov(x, t)' = DCov(x, t) BY <2>1 DEF DCov
    <3> QED BY <2>5, <3>0, <3>1, <3>2, <3>3 DEF OpenOK
  <2>9. \A j \in ReadIds \ {i} : (rd'[j].st = "open" => OpenOK(j)') /\ (rd'[j].st = "ready" =>
            \A t \in rd'[j].before : t < rd'[j].applied)
    BY <2>1, <2>2, <2>6, <2>7, OthersKept
  <2>10. \A s \in Slots : decided'[s] # None => Covered(s)' BY <2>1, <2>7, CoveredKept DEF FInv
  <2> QED BY <2>5, <2>6, <2>8, <2>9, <2>10 DEF FInv, ReadSafe
<1>12. ASSUME NEW i \in ReadIds, Ready(i) PROVE FInv'
  <2> DEFINE r == rd[i].at
  <2>1. rd' = [rd EXCEPT ![i].st = "ready", ![i].applied = Prefix(log[r])]
        /\ rd[i].st = "open"
        /\ Prefix(log[r]) >= rd[i].k
        /\ \A E \in Covering(r, Prefix(log[r])) : 2 * Cardinality(rd[i].who \cap E) > Cardinality(E)
        /\ UNCHANGED <<decided, log, dlog, retired, vreg, reg, floor>>
    BY <1>12 DEF Ready
  <2>u. UNCHANGED <<decided, log, dlog, retired, vreg, reg, floor>> BY <1>12 DEF Ready
  <2>0. rd[i] \in ReadState BY <1>t DEF RInvTypes
  <2>2. /\ rd'[i] = [rd[i] EXCEPT !.st = "ready", !.applied = Prefix(log[r])]
        /\ \A j \in ReadIds \ {i} : rd'[j] = rd[j]
    BY <2>1, <1>t DEF RInvTypes
  <2>3. r \in Replicas BY <2>1 DEF FInv, OpenOK
  <2>4. Prefix(log[r]) \in 0..N BY <2>3, <1>t, PrefixProps DEF RInvTypes
  <2>5. /\ rd'[i].st = "ready" /\ rd'[i].before = rd[i].before /\ rd'[i].applied = Prefix(log[r])
    BY <2>0, <2>2 DEF ReadState
  <2>6. 2 * Cardinality(rd[i].who \cap InitMembers) > Cardinality(InitMembers)
    BY <2>1, <2>3, <2>4, <1>t, CoveringFixed
  <2>7. \A t \in rd[i].before : t < Prefix(log[r])
    <3> SUFFICES ASSUME NEW t \in rd[i].before PROVE t < Prefix(log[r]) OBVIOUS
    <3>1. PICK Q \in Maj(InitMembers) :
              /\ \A x \in Q : DCov(x, t)
              /\ \A x \in Q \cap rd[i].who : t < rd[i].k
      BY <2>1 DEF FInv, OpenOK
    <3>2. IsFiniteSet(InitMembers) BY FixedAssumptions, FS_Subset
    <3>3. Q \cap (rd[i].who \cap InitMembers) # {}
      BY <3>1, <3>2, <2>6, QuorumsMeet DEF Maj
    <3>4. t < rd[i].k BY <3>1, <3>3
    <3>5. t \in Nat /\ rd[i].k \in Nat BY <2>0, MAssumptions DEF ReadState, Slots
    <3> QED BY <2>1, <2>4, <3>4, <3>5
  <2>8. RInvTypes'
    <3>1. rd'[i] \in ReadState BY <2>0, <2>2, <2>4, <1>t DEF ReadState, RInvTypes
    <3>2. rd' \in [ReadIds -> ReadState] BY <3>1, <2>1, <2>2, <1>t DEF RInvTypes
    <3> QED BY <3>2, <2>u, <1>t DEF RInvTypes, Decided
  <2>9. DCovKeep
    <3>1. floor' = floor /\ reg' = reg /\ dlog' = dlog BY <2>u
    <3> QED BY <3>1 DEF DCovKeep, DCov
  <2>10. \A j \in ReadIds \ {i} : (rd'[j].st = "open" => OpenOK(j)') /\ (rd'[j].st = "ready" =>
            \A t \in rd'[j].before : t < rd'[j].applied)
    BY <2>u, <2>2, <2>8, <2>9, OthersKept
  <2>11. \A s \in Slots : decided'[s] # None => Covered(s)' BY <2>u, <2>9, CoveredKept DEF FInv
  <2> QED BY <2>5, <2>7, <2>8, <2>10, <2>11 DEF FInv, ReadSafe
<1>13. CASE UNCHANGED allvars
  <2>1. RInvTypes' BY <1>13, <1>t DEF allvars, RInvTypes, Decided
  <2>2. DCovKeep BY <1>13, DCovGrow DEF allvars
  <2> QED BY <1>13, <2>1, <2>2, ReadsKept DEF allvars
<1> QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6, <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13
    DEF RNext

THEOREM ReadsFixedSafe == RSpec => []ReadSafe
<1>1. RInit => FInv BY InitFInv
<1>2. FInv /\ [RNext]_allvars => FInv' BY FInvNext
<1>3. FInv => ReadSafe BY DEF FInv
<1> QED BY <1>1, <1>2, <1>3, PTL DEF RSpec
=============================================================================

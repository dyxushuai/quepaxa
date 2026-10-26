------------------------- MODULE MembershipRecover -------------------------
(***************************************************************************)
(* TLAPS proof of Membership.tla's Recoverable: retiring replicas never    *)
(* lose a decided slot, even with a tolerable set of replicas down as     *)
(* well, for any number of replicas, slots, members and changes.          *)
(*                                                                         *)
(* A decided slot is held by its own members (their registers) until one  *)
(* of them retires; a retirement hands every slot before the new epoch to *)
(* a majority of that epoch, which durably hold them. Custody records     *)
(* the hand-over at a point of the log that is fully decided, so later    *)
(* decisions never change it.                                              *)
(***************************************************************************)
EXTENDS MembershipProof, FiniteSetTheorems

ASSUME RAssumptions ==
    /\ IsFiniteSet(Replicas)
    /\ InitMembers \subseteq Replicas
    /\ Changes \subseteq SUBSET Replicas

-----------------------------------------------------------------------------
(* The schedule: where members come from *)

\* From a point k at which the epoch being installed is in force, the members of any later
\* slot are that epoch's or those of a change at a slot from k on.
LEMMA Sources ==
    ASSUME NEW val, NEW k \in Nat, Folded(val)[k].from <= k, NEW u \in Nat, k <= u
    PROVE  /\ Folded(val)[u].nxt \in {Folded(val)[k].nxt} \cup {val[t].m : t \in k..(u - 1)}
           /\ Eff(Folded(val)[u], u) \in {Folded(val)[k].nxt} \cup {val[t].m : t \in k..(u - 1)}
<1> DEFINE S(n) == {Folded(val)[k].nxt} \cup {val[t].m : t \in k..(n - 1)}
           C(j) == /\ Folded(val)[k + j].nxt \in S(k + j)
                   /\ Folded(val)[k + j].from > k + j => Folded(val)[k + j].cur \in S(k + j)
<1>1. C(0)
  <2>1. k + 0 = k OBVIOUS
  <2>2. Folded(val)[k].nxt \in S(k) OBVIOUS
  <2>3. ~(Folded(val)[k].from > k) BY FoldedShape DEF IsSt
  <2> QED BY <2>1, <2>2, <2>3
<1>2. \A j \in Nat : C(j) => C(j + 1)
  <2> SUFFICES ASSUME NEW j \in Nat, C(j) PROVE C(j + 1) OBVIOUS
  <2> DEFINE n == k + j  st == Folded(val)[n]
             act == IF st.from <= n THEN [st EXCEPT !.cur = st.nxt] ELSE st
  <2>1. Folded(val)[n + 1] = FoldStep(st, n, val) /\ IsSt(st) /\ n \in Nat BY FoldedDef, FoldedShape
  <2>2. S(n) \subseteq S(n + 1) /\ val[n].m \in S(n + 1) OBVIOUS
  <2>3. act.nxt \in S(n) /\ act.from = st.from /\ (act.from > n => act.cur \in S(n))
        /\ (st.from <= n => act.cur \in S(n))
    BY <2>1 DEF IsSt
  <2>4. act.cur \in S(n) BY <2>3, <2>1 DEF IsSt
  <2>5. CASE val[n].k = "change" /\ act.from <= n
    <3>1. Folded(val)[n + 1] = [act EXCEPT !.nxt = val[n].m, !.from = n + LAG]
      BY <2>1, <2>5 DEF FoldStep
    <3> QED BY <3>1, <2>2, <2>4, <2>1 DEF IsSt
  <2>6. CASE ~(val[n].k = "change" /\ act.from <= n)
    <3>1. Folded(val)[n + 1] = act BY <2>1, <2>6 DEF FoldStep
    <3>2. Folded(val)[n + 1].from > n + 1 => st.from > n BY <3>1, <2>3, <2>1 DEF IsSt
    <3> QED BY <3>1, <3>2, <2>2, <2>3, <2>1 DEF IsSt
  <2> QED BY <2>5, <2>6
<1> HIDE DEF C
<1>3. \A j \in Nat : C(j) BY <1>1, <1>2, NatInduction
<1>4. C(u - k) BY <1>3
<1>5. Folded(val)[u].nxt \in S(u) /\ (Folded(val)[u].from > u => Folded(val)[u].cur \in S(u))
  BY <1>4 DEF C
<1>6. IsSt(Folded(val)[u]) BY FoldedShape
<1> QED BY <1>5, <1>6 DEF Eff, IsSt

\* Member sets are sets of replicas, as far as the slots go.
LEMMA FoldedSubsets ==
    ASSUME NEW val, \A t \in Slots : val[t].m \subseteq Replicas
    PROVE  \A n \in 0..N : Folded(val)[n].cur \subseteq Replicas /\ Folded(val)[n].nxt \subseteq Replicas
<1> DEFINE P(n) == n <= N => Folded(val)[n].cur \subseteq Replicas /\ Folded(val)[n].nxt \subseteq Replicas
<1>1. P(0) BY FoldedDef, RAssumptions DEF Start
<1>2. \A n \in Nat : P(n) => P(n + 1)
  <2> SUFFICES ASSUME NEW n \in Nat, P(n), n + 1 <= N
               PROVE  Folded(val)[n + 1].cur \subseteq Replicas /\ Folded(val)[n + 1].nxt \subseteq Replicas
    OBVIOUS
  <2> DEFINE st == Folded(val)[n]
             act == IF st.from <= n THEN [st EXCEPT !.cur = st.nxt] ELSE st
  <2>1. Folded(val)[n + 1] = FoldStep(st, n, val) /\ IsSt(st) BY FoldedDef, FoldedShape
  <2>2. val[n].m \subseteq Replicas BY MAssumptions DEF Slots
  <2>3. st.cur \subseteq Replicas /\ st.nxt \subseteq Replicas BY MAssumptions
  <2>4. act.cur \subseteq Replicas /\ act.nxt \subseteq Replicas BY <2>1, <2>3 DEF IsSt
  <2>5. CASE val[n].k = "change" /\ act.from <= n
    <3>1. Folded(val)[n + 1] = [act EXCEPT !.nxt = val[n].m, !.from = n + LAG]
      BY <2>1, <2>5 DEF FoldStep
    <3> QED BY <3>1, <2>2, <2>4, <2>1 DEF IsSt
  <2>6. CASE ~(val[n].k = "change" /\ act.from <= n)
    <3>1. Folded(val)[n + 1] = act BY <2>1, <2>6 DEF FoldStep
    <3> QED BY <3>1, <2>4
  <2> QED BY <2>5, <2>6
<1> HIDE DEF P
<1>3. \A n \in Nat : P(n) BY <1>1, <1>2, NatInduction
<1> QED BY <1>3, MAssumptions DEF P

\* The members of slot s and after, and of a pending change: the sets Tolerable bounds.
Later(val, s) == {MembersAt(t, val) : t \in s..N} \cup {Folded(val)[N].cur, Folded(val)[N].nxt}

\* A custody witness's epoch is one of them.
LEMMA WitnessEpoch ==
    ASSUME NEW val, NEW s \in Nat, NEW k \in 0..N,
           s < Folded(val)[k].from, Folded(val)[k].from < k + LAG
    PROVE  Folded(val)[k].nxt \in Later(val, s)
<1> DEFINE sk == Folded(val)[k]  f == sk.from
<1>0. IsSt(sk) /\ f \in Nat /\ N \in Nat BY FoldedShape, MAssumptions DEF IsSt
<1>1. CASE f <= k
  <2>1. MembersAt(k, val) = sk.nxt BY <1>1 DEF MembersAt
  <2> QED BY <1>0, <1>1, <2>1 DEF Later
<1>2. CASE k < f /\ f <= N
  <2>1. Eff(Folded(val)[f], f) = Eff(Folded(val)[k], f) BY <1>0, <1>2, WindowEff
  <2>2. MembersAt(f, val) = sk.nxt BY <2>1, <1>0 DEF MembersAt, Eff
  <2> QED BY <1>0, <1>2, <2>2 DEF Later
<1>3. CASE f > N
  <2>1. Folded(val)[N] = sk BY <1>0, <1>3, PendingSteps
  <2> QED BY <2>1 DEF Later
<1> QED BY <1>0, <1>1, <1>2, <1>3

\* With nothing decided from p on, every member set from the latest epoch's first slot on
\* is that epoch.
LEMMA TailEqual ==
    ASSUME NEW val, NEW p \in 0..N, Folded(val)[p].from <= p,
           \A t \in p..(N - 1) : val[t].k # "change",
           NEW s \in Nat, Folded(val)[p].from <= s, s < N
    PROVE  \A E \in Later(val, s) : E = Folded(val)[p].nxt
<1> DEFINE f == Folded(val)[p].from  E1 == Folded(val)[p].nxt
<1>0. IsSt(Folded(val)[p]) /\ f \in Nat /\ N \in Nat BY FoldedShape, MAssumptions DEF IsSt
<1>1. \A j \in s..N : MembersAt(j, val) = E1
  <2> SUFFICES ASSUME NEW j \in s..N PROVE MembersAt(j, val) = E1 OBVIOUS
  <2>1. CASE j <= p
    <3>1. Folded(val)[j].nxt = E1 /\ Folded(val)[j].from = f BY <1>0, <2>1, Mono
    <3> QED BY <3>1, <1>0 DEF MembersAt
  <2>2. CASE j > p
    <3>1. Folded(val)[j].nxt = E1 /\ Folded(val)[j].from = f BY <1>0, <2>2, WriteSteps
    <3> QED BY <3>1, <1>0 DEF MembersAt
  <2> QED BY <2>1, <2>2, <1>0
<1>2. Folded(val)[N].nxt = E1 /\ Folded(val)[N].from = f BY <1>0, WriteSteps
<1>3. Folded(val)[N].cur = E1 BY <1>0, <1>2, InForce
<1> QED BY <1>1, <1>2, <1>3 DEF Later

-----------------------------------------------------------------------------
(* Majorities and minorities *)

LEMMA MajorityOutlivesMinority ==
    ASSUME NEW E, IsFiniteSet(E), NEW Q \in SUBSET E, 2 * Cardinality(Q) > Cardinality(E),
           NEW D, 2 * Cardinality(D \cap E) < Cardinality(E)
    PROVE  Q \ D # {}
<1> SUFFICES ASSUME Q \subseteq D PROVE FALSE OBVIOUS
<1>1. Q \subseteq D \cap E OBVIOUS
<1>2. IsFiniteSet(D \cap E) /\ Cardinality(Q) <= Cardinality(D \cap E) BY <1>1, FS_Subset, FS_Intersection
<1>3. Cardinality(Q) \in Nat /\ Cardinality(D \cap E) \in Nat /\ Cardinality(E) \in Nat
  BY <1>2, FS_Subset, FS_CardinalityType
<1> QED BY <1>2, <1>3

LEMMA MinorityLeavesMajority ==
    ASSUME NEW M, IsFiniteSet(M), NEW D, 2 * Cardinality(D \cap M) < Cardinality(M)
    PROVE  2 * Cardinality(M \ D) > Cardinality(M)
<1>1. Cardinality(M \ D) = Cardinality(M) - Cardinality(M \cap D) BY FS_Difference
<1>2. M \cap D = D \cap M OBVIOUS
<1>3. Cardinality(M) \in Nat /\ Cardinality(D \cap M) \in Nat
  BY FS_CardinalityType, FS_Intersection
<1> QED BY <1>1, <1>2, <1>3
-----------------------------------------------------------------------------
(* The invariant *)

DecidedBelow == \A t \in Slots : decided[t] # None => \A u \in 0..(t - LAG) : decided[u] # None

\* Every slot before k is decided, so the schedule up to k never changes.
StablePoint(k) == k \in 0..N /\ \A w \in 0..(k - 1) : decided[w] # None

\* Retired x is out for good: from a stable point at which its removal is in force, no
\* member set names it.
RetiredAt(x, k) ==
    /\ StablePoint(k)
    /\ Folded(Truth)[k].from <= k
    /\ x \notin Folded(Truth)[k].nxt
    /\ \A t \in k..(N - 1) : x \notin Truth[t].m

\* Slot s was handed to the epoch installed by stable point k: a majority of it, none
\* retired, durably holds s.
Witness(s, k) ==
    /\ StablePoint(k)
    /\ s < Folded(Truth)[k].from
    /\ Folded(Truth)[k].from < k + LAG
    /\ \E Q \in Maj(Folded(Truth)[k].nxt) : \A q \in Q : q \notin retired /\ s \in dlog[q]

Custody ==
    \A s \in Slots : decided[s] # None =>
        \/ MembersAt(s, Truth) \cap retired = {}
        \/ \E k \in 0..N : Witness(s, k)

RInv ==
    /\ Inv
    /\ retired \subseteq Replicas
    /\ \A r \in Replicas : dlog[r] \subseteq log[r]
    /\ DecidedBelow
    /\ \A x \in retired : \E k \in 0..N : RetiredAt(x, k)
    /\ Custody

LEMMA TruthFacts ==
    ASSUME Inv
    PROVE  /\ \A t \in Slots : Truth[t] = IF decided[t] = None THEN Write ELSE decided[t]
           /\ \A t \in Slots : Truth[t].m \subseteq Replicas
           /\ \A t \in Slots : decided[t] = None => Truth[t].k # "change" /\ Truth[t].m = {}
<1>1. \A v \in Values : v.m \subseteq Replicas BY RAssumptions DEF Values, Write, Change
<1> QED BY <1>1 DEF Inv, Truth, Write

\* Retired replicas name no member set still to come.
LEMMA RetiredOut ==
    ASSUME RInv, NEW x \in retired
    PROVE  /\ x \notin Current.nxt
           /\ \A s \in Slots : decided[s] = None => x \notin MembersAt(s, Truth)
<1>0. PICK tv : tv = Truth OBVIOUS
<1>1. PICK k \in 0..N : RetiredAt(x, k) BY DEF RInv
<1>2. /\ \A w \in 0..(k - 1) : decided[w] # None
      /\ Folded(tv)[k].from <= k
      /\ x \notin Folded(tv)[k].nxt
      /\ \A t \in k..(N - 1) : x \notin tv[t].m
  BY <1>0, <1>1 DEF RetiredAt, StablePoint
<1>3. k \in Nat /\ N \in Nat BY MAssumptions
<1>a. k <= N BY <1>3
\* Sources from k: x is named by no source, so by no later member set.
<1>c. \A uu \in Nat : k <= uu /\ uu <= N => x \notin Folded(tv)[uu].nxt /\ x \notin Eff(Folded(tv)[uu], uu)
  <2> SUFFICES ASSUME NEW uu \in Nat, k <= uu, uu <= N
               PROVE  x \notin Folded(tv)[uu].nxt /\ x \notin Eff(Folded(tv)[uu], uu)
    OBVIOUS
  <2>1. /\ Folded(tv)[uu].nxt \in {Folded(tv)[k].nxt} \cup {tv[t].m : t \in k..(uu - 1)}
        /\ Eff(Folded(tv)[uu], uu) \in {Folded(tv)[k].nxt} \cup {tv[t].m : t \in k..(uu - 1)}
    <3>1. Folded(tv)[k].from <= k /\ k \in Nat BY <1>2, <1>3
    <3> QED BY <3>1, Sources
  <2>2. \A t \in k..(uu - 1) : x \notin tv[t].m BY <1>2, <1>3
  <2> QED BY <1>2, <2>1, <2>2
<1>4. x \notin Folded(tv)[N].nxt BY <1>3, <1>a, <1>c
<1>5. \A s \in Slots : decided[s] = None => x \notin MembersAt(s, tv)
  <2> SUFFICES ASSUME NEW s \in Slots, decided[s] = None PROVE x \notin MembersAt(s, tv) OBVIOUS
  <2>1. k <= s /\ s \in Nat BY <1>2, <1>3 DEF Slots
  <2>4. x \notin Eff(Folded(tv)[s], s) BY <1>3, <2>1, <1>c, MAssumptions DEF Slots
  <2> QED BY <2>4 DEF MembersAt, Eff
<1> QED BY <1>0, <1>4, <1>5 DEF Current

-----------------------------------------------------------------------------
(* Recoverable *)

THEOREM InvRecoverable == RInv => Recoverable
<1> SUFFICES ASSUME RInv, NEW s \in Slots, decided[s] # None, NEW D \in SUBSET Replicas,
                    Tolerable(s, D)
             PROVE  LET gone == retired \cup D
                    IN \/ \E r \in Replicas \ gone : s \in dlog[r]
                       \/ 2 * Cardinality(MembersAt(s, Truth) \ gone) > Cardinality(MembersAt(s, Truth))
  BY DEF Recoverable
<1>0. PICK tv : tv = Truth OBVIOUS
<1>1. \A t \in Slots : tv[t].m \subseteq Replicas BY <1>0, TruthFacts DEF RInv
<1>2. \A n \in 0..N : Folded(tv)[n].cur \subseteq Replicas /\ Folded(tv)[n].nxt \subseteq Replicas
  BY <1>1, FoldedSubsets
<1>3. LaterEpochs(s) = Later(tv, s) BY <1>0 DEF LaterEpochs, Later, Current
<1>4. \A E \in LaterEpochs(s) : 2 * Cardinality(D \cap E) < Cardinality(E) BY DEF Tolerable
<1>5. s \in Nat /\ s <= N /\ N \in Nat BY MAssumptions DEF Slots
<1>6. CASE MembersAt(s, Truth) \cap retired = {}
  <2> DEFINE M == MembersAt(s, Truth)
  <2>1. M \in LaterEpochs(s) BY <1>5 DEF LaterEpochs
  <2>2. M \subseteq Replicas /\ IsFiniteSet(M)
    BY <1>0, <1>2, <1>5, RAssumptions, FS_Subset DEF MembersAt
  <2>3. 2 * Cardinality(M \ D) > Cardinality(M) BY <1>4, <2>1, <2>2, MinorityLeavesMajority
  <2>4. M \ (retired \cup D) = M \ D BY <1>6
  <2> QED BY <2>3, <2>4
<1>7. CASE \E k \in 0..N : Witness(s, k)
  <2>1. PICK k \in 0..N : Witness(s, k) BY <1>7
  <2> DEFINE E == Folded(tv)[k].nxt
  <2>2. /\ s < Folded(tv)[k].from /\ Folded(tv)[k].from < k + LAG
        /\ \E Q \in Maj(E) : \A q \in Q : q \notin retired /\ s \in dlog[q]
    BY <1>0, <2>1 DEF Witness
  <2>3. E \in LaterEpochs(s) BY <1>3, <1>5, <2>2, WitnessEpoch
  <2>4. PICK Q \in Maj(E) : \A q \in Q : q \notin retired /\ s \in dlog[q] BY <2>2
  <2>5. E \subseteq Replicas /\ IsFiniteSet(E) BY <1>2, RAssumptions, FS_Subset
  <2>6. Q \ D # {} BY <1>4, <2>3, <2>4, <2>5, MajorityOutlivesMinority DEF Maj
  <2>7. PICK q \in Q \ D : TRUE BY <2>6
  <2> QED BY <2>4, <2>5, <2>7 DEF Maj
<1> QED BY <1>6, <1>7 DEF RInv, Custody
-----------------------------------------------------------------------------
(* RInv is inductive *)

THEOREM RInvInit == Init => RInv
<1> SUFFICES ASSUME Init PROVE RInv OBVIOUS
<1>1. Inv BY InitInv
<1>2. \A t \in Slots : decided[t] = None BY DEF Init
<1> QED BY <1>1, <1>2 DEF Init, RInv, DecidedBelow, Custody

\* Deciding slot s changes the truth only at s, which no stable point covers.
LEMMA DecideTruth ==
    ASSUME RInv, NEW r \in Replicas, NEW s \in Slots, NEW v \in Values, Decide(r, s, v),
           NEW tv, tv = Truth, NEW tv2, tv2 = Truth'
    PROVE  /\ \A t \in Slots \ {s} : tv2[t] = tv[t]
           /\ tv2[s] = v
           /\ \A k \in 0..N : (\A w \in 0..(k - 1) : decided[w] # None) => Folded(tv2)[k] = Folded(tv)[k]
           /\ \A t \in Slots : decided[t] # None => MembersAt(t, tv2) = MembersAt(t, tv)
           /\ MembersAt(s, tv2) = MembersAt(s, tv)
<1>0. \A u \in Values : u # None BY DEF Values, Write, Change, None
<1>1. decided' = [decided EXCEPT ![s] = v] /\ decided[s] = None BY DEF Decide
<1>2. \A t \in Slots \ {s} : tv2[t] = tv[t] BY <1>1 DEF RInv, Inv, Truth
<1>3. tv2[s] = v BY <1>0, <1>1 DEF RInv, Inv, Truth
<1>4. s \in Nat /\ N \in Nat BY MAssumptions DEF Slots
<1>5. \A k \in 0..N : (\A w \in 0..(k - 1) : decided[w] # None) => Folded(tv2)[k] = Folded(tv)[k]
  <2> SUFFICES ASSUME NEW k \in 0..N, \A w \in 0..(k - 1) : decided[w] # None
               PROVE  Folded(tv2)[k] = Folded(tv)[k]
    OBVIOUS
  <2>1. \A w \in 0..(k - 1) : tv2[w] = tv[w] BY <1>1, <1>2, MAssumptions DEF Slots
  <2> QED BY <2>1, <1>4, PrefixAgree
<1>6. \A t \in Slots : decided[t] # None => MembersAt(t, tv2) = MembersAt(t, tv)
  <2> SUFFICES ASSUME NEW t \in Slots, decided[t] # None PROVE MembersAt(t, tv2) = MembersAt(t, tv)
    OBVIOUS
  <2>0. t \in Nat BY MAssumptions DEF Slots
  <2>1. CASE s >= t
    <3>1. \A w \in 0..(t - 1) : tv2[w] = tv[w] BY <2>1, <1>2, MAssumptions DEF Slots
    <3>2. Folded(tv2)[t] = Folded(tv)[t] BY <3>1, <2>0, PrefixAgree
    <3> QED BY <3>2 DEF MembersAt
  <2>2. CASE s < t
    <3>1. s > t - LAG BY <1>1, <2>2, <1>4, <2>0, MAssumptions DEF RInv, DecidedBelow
    <3>2. \A w \in 0..(s - 1) : tv2[w] = tv[w] BY <1>2, MAssumptions DEF Slots
    <3>3. Folded(tv2)[s] = Folded(tv)[s] BY <3>2, <1>4, PrefixAgree
    <3>4. s <= t /\ t < s + LAG BY <2>2, <3>1, <1>4, <2>0, MAssumptions
    <3>5. Eff(Folded(tv2)[t], t) = Eff(Folded(tv2)[s], t) BY <3>4, <1>4, <2>0, WindowEff
    <3>6. Eff(Folded(tv)[t], t) = Eff(Folded(tv)[s], t) BY <3>4, <1>4, <2>0, WindowEff
    <3> QED BY <3>3, <3>5, <3>6 DEF MembersAt, Eff
  <2> QED BY <2>1, <2>2, <1>4, <2>0
<1>7. MembersAt(s, tv2) = MembersAt(s, tv)
  <2>1. \A w \in 0..(s - 1) : tv2[w] = tv[w] BY <1>2, MAssumptions DEF Slots
  <2>2. Folded(tv2)[s] = Folded(tv)[s] BY <2>1, <1>4, PrefixAgree
  <2> QED BY <2>2 DEF MembersAt
<1> QED BY <1>2, <1>3, <1>5, <1>6, <1>7

LEMMA DecideStep ==
    ASSUME RInv, NEW r \in Replicas, NEW s \in Slots, NEW v \in Values, Decide(r, s, v)
    PROVE  RInv'
<1>0. PICK tv : tv = Truth OBVIOUS
<1>a. PICK tv2 : tv2 = Truth' OBVIOUS
<1>1. /\ \A t \in Slots \ {s} : tv2[t] = tv[t]
      /\ tv2[s] = v
      /\ \A k \in 0..N : (\A w \in 0..(k - 1) : decided[w] # None) => Folded(tv2)[k] = Folded(tv)[k]
      /\ \A t \in Slots : decided[t] # None => MembersAt(t, tv2) = MembersAt(t, tv)
      /\ MembersAt(s, tv2) = MembersAt(s, tv)
  BY <1>0, <1>a, DecideTruth
<1>2. /\ decided' = [decided EXCEPT ![s] = v] /\ decided[s] = None
      /\ log' = [log EXCEPT ![r] = @ \cup {s}] /\ UNCHANGED <<dlog, retired>>
      /\ v.m \cap retired = {} /\ CanDrive(r, s)
  BY DEF Decide
<1>3. \A t \in Slots : decided[t] # None => decided'[t] = decided[t] BY <1>2 DEF RInv, Inv
<1>4. \A t \in Slots : decided'[t] # None <=> (decided[t] # None \/ t = s)
  BY <1>2, Values # {} DEF RInv, Inv, Values, Write, Change, None
<1>5. Inv' BY NextInv DEF RInv, Next
<1>6. DecidedBelow'
  <2> SUFFICES ASSUME NEW t \in Slots, decided'[t] # None, NEW u \in 0..(t - LAG)
               PROVE  decided'[u] # None
    BY DEF DecidedBelow
  <2>1. CASE t = s
    <3>1. s < Prefix(log[r]) + LAG BY <1>2 DEF CanDrive
    <3>2. Prefix(log[r]) \in 0..N /\ \A w \in 0..(Prefix(log[r]) - 1) : w \in log[r]
      BY PrefixProps DEF RInv, Inv
    <3>3. u \in log[r] BY <2>1, <3>1, <3>2, MAssumptions DEF Slots
    <3>4. u \in Slots /\ decided[u] # None BY <3>3 DEF RInv, Inv, Decided
    <3> QED BY <3>4, <1>4
  <2>2. CASE t # s
    <3>1. decided[t] # None BY <2>2, <1>4
    <3>2. decided[u] # None /\ u \in Slots BY <3>1, MAssumptions DEF RInv, DecidedBelow, Slots
    <3> QED BY <3>2, <1>4
  <2> QED BY <2>1, <2>2
<1>7. \A x \in retired' : \E k \in 0..N : RetiredAt(x, k)'
  <2> SUFFICES ASSUME NEW x \in retired PROVE \E k \in 0..N : RetiredAt(x, k)' BY <1>2
  <2>1. PICK k \in 0..N : RetiredAt(x, k) BY DEF RInv
  <2>2. /\ \A w \in 0..(k - 1) : decided[w] # None
        /\ Folded(tv)[k].from <= k
        /\ x \notin Folded(tv)[k].nxt
        /\ \A t \in k..(N - 1) : x \notin tv[t].m
    BY <1>0, <2>1 DEF RetiredAt, StablePoint
  <2>3. Folded(tv2)[k] = Folded(tv)[k] BY <1>1, <2>2
  <2>4. \A w \in 0..(k - 1) : decided'[w] # None BY <2>2, <1>4, MAssumptions DEF Slots
  <2>5. \A t \in k..(N - 1) : x \notin tv2[t].m BY <1>1, <1>2, <2>2, MAssumptions DEF Slots
  <2> QED BY <1>a, <2>3, <2>4, <2>5, <2>2 DEF RetiredAt, StablePoint
<1>8. Custody'
  <2> SUFFICES ASSUME NEW t \in Slots, decided'[t] # None
               PROVE  \/ MembersAt(t, Truth)' \cap retired' = {}
                      \/ \E k \in 0..N : Witness(t, k)'
    BY DEF Custody
  <2>1. CASE t = s
    <3>1. decided[s] = None BY <1>2
    <3>2. \A x \in retired : x \notin MembersAt(s, tv) BY <3>1, <1>0, RetiredOut
    <3>3. MembersAt(s, tv2) \cap retired = {} BY <3>2, <1>1
    <3> QED BY <2>1, <3>3, <1>a, <1>2
  <2>2. CASE t # s
    <3>1. decided[t] # None BY <2>2, <1>4
    <3>2. CASE MembersAt(t, Truth) \cap retired = {}
      BY <3>1, <3>2, <1>0, <1>a, <1>1, <1>2
    <3>3. CASE \E k \in 0..N : Witness(t, k)
      <4>1. PICK k \in 0..N : Witness(t, k) BY <3>3
      <4>2. /\ \A w \in 0..(k - 1) : decided[w] # None
            /\ t < Folded(tv)[k].from /\ Folded(tv)[k].from < k + LAG
            /\ \E Q \in Maj(Folded(tv)[k].nxt) : \A q \in Q : q \notin retired /\ t \in dlog[q]
        BY <1>0, <4>1 DEF Witness, StablePoint
      <4>3. Folded(tv2)[k] = Folded(tv)[k] BY <1>1, <4>2
      <4>4. \A w \in 0..(k - 1) : decided'[w] # None BY <4>2, <1>4, MAssumptions DEF Slots
      <4> QED BY <1>a, <1>2, <4>2, <4>3, <4>4 DEF Witness, StablePoint
    <3> QED BY <3>1, <3>2, <3>3 DEF RInv, Custody
  <2> QED BY <2>1, <2>2
<1>9. retired' \subseteq Replicas /\ \A x \in Replicas : dlog'[x] \subseteq log'[x]
  BY <1>2 DEF RInv, Inv
<1> QED BY <1>5, <1>6, <1>7, <1>8, <1>9 DEF RInv
\* A retirement hands every slot before the new epoch to a majority of it.
LEMMA RetireStep ==
    ASSUME RInv, NEW r \in Replicas, Retire(r)
    PROVE  RInv'
<1>0. PICK tv : tv = Truth OBVIOUS
<1>b. PICK kv : kv = Known(r) OBVIOUS
\* p is a constant, so priming leaves it alone.
<1>c. PICK p : p = Prefix(log[r]) OBVIOUS
<1> DEFINE st == Folded(kv)[p]
           f == st.from
           caught == {m \in st.nxt : Prefix(dlog[m]) >= f}
<1>1. /\ r \notin retired /\ f > 0 /\ r \notin st.nxt
      /\ 2 * Cardinality(caught) > Cardinality(st.nxt)
      /\ \A t \in Slots : decided[t] # None => t < p
      /\ retired' = retired \cup {r} /\ UNCHANGED <<decided, log, dlog>>
  BY <1>b, <1>c DEF Retire, Latest
<1>2. p \in 0..N /\ \A t \in 0..(p - 1) : t \in log[r] /\ decided[t] # None
  BY <1>c, PrefixProps DEF RInv, Inv, Decided
<1>3. Folded(kv)[p] = Folded(tv)[p]
  <2>1. \A t \in 0..(p - 1) : kv[t] = tv[t] BY <1>0, <1>b, <1>2, MAssumptions DEF Known, Truth, Slots
  <2> QED BY <2>1, <1>2, MAssumptions, PrefixAgree
<1>4. \A t \in p..(N - 1) : tv[t].k # "change" /\ tv[t].m = {}
  <2> SUFFICES ASSUME NEW t \in p..(N - 1) PROVE tv[t].k # "change" /\ tv[t].m = {} OBVIOUS
  <2>1. t \in Slots /\ decided[t] = None BY <1>1, <1>2, MAssumptions DEF Slots
  <2> QED BY <2>1, <1>0, TruthFacts DEF RInv
<1>5. /\ \A t \in Slots : tv[t].m \subseteq Replicas
      /\ st.nxt \subseteq Replicas /\ IsFiniteSet(st.nxt) /\ caught # {}
  <2>1. \A t \in Slots : tv[t].m \subseteq Replicas BY <1>0, TruthFacts DEF RInv
  <2>2. st.nxt \subseteq Replicas BY <2>1, <1>2, <1>3, FoldedSubsets
  <2>3. IsFiniteSet(st.nxt) BY <2>2, RAssumptions, FS_Subset
  <2>4. caught # {}
    <3> SUFFICES ASSUME caught = {} PROVE FALSE OBVIOUS
    <3>1. Cardinality(st.nxt) \in Nat BY <2>3, FS_CardinalityType
    <3> QED BY <1>1, <3>1, FS_EmptySet
  <2> QED BY <2>1, <2>2, <2>3, <2>4
<1>6. f \in Nat /\ f < p + LAG /\ f <= p
  <2>1. f \in Nat /\ f < p + LAG BY <1>2, MAssumptions, FromBound, FoldedShape DEF IsSt
  <2>2. PICK m \in caught : TRUE BY <1>5
  <2>3. Prefix(dlog[m]) >= f /\ m \in Replicas BY <1>5
  <2>4. Prefix(dlog[m]) \in 0..N /\ \A w \in 0..(Prefix(dlog[m]) - 1) : w \in dlog[m]
    BY <2>3, PrefixProps DEF RInv, Inv
  <2>5. \A w \in 0..(f - 1) : decided[w] # None /\ w \in Slots
    BY <2>1, <2>3, <2>4, MAssumptions DEF RInv, Inv, Decided
  <2>6. f <= p
    <3> SUFFICES ASSUME f > p PROVE FALSE BY <2>1, <1>2, MAssumptions
    <3>1. p \in 0..(f - 1) BY <2>1, <1>2, MAssumptions
    <3>2. decided[p] # None /\ p \in Slots BY <2>5, <3>1
    <3> QED BY <3>2, <1>1, <1>2, MAssumptions
  <2> QED BY <2>1, <2>6
<1>7. Folded(tv)[N].nxt = st.nxt BY <1>2, <1>3, <1>4, MAssumptions, WriteSteps
<1>8. st.nxt \cap retired = {}
  <2> SUFFICES ASSUME NEW x \in retired, x \in st.nxt PROVE FALSE OBVIOUS
  <2> QED BY <1>0, <1>7, RetiredOut DEF Current
<1>9. \A q \in caught : q \notin retired' BY <1>1, <1>8
<1>10. \A x \in retired' : \E k \in 0..N : RetiredAt(x, k)'
  <2> SUFFICES ASSUME NEW x \in retired' PROVE \E k \in 0..N : RetiredAt(x, k)' OBVIOUS
  <2>1. CASE x = r
    <3>1. RetiredAt(r, p) BY <1>0, <1>2, <1>3, <1>4, <1>6, <1>1 DEF RetiredAt, StablePoint
    <3> QED BY <3>1, <1>1, <1>2, <2>1 DEF RetiredAt, StablePoint, Truth
  <2>2. CASE x # r
    <3>1. PICK k \in 0..N : RetiredAt(x, k) BY <2>2, <1>1 DEF RInv
    <3> QED BY <3>1, <1>1 DEF RetiredAt, StablePoint, Truth
  <2> QED BY <2>1, <2>2
<1>11. Custody'
  <2> SUFFICES ASSUME NEW s \in Slots, decided[s] # None
               PROVE  \/ MembersAt(s, Truth) \cap retired' = {}
                      \/ \E k \in 0..N : Witness(s, k)'
    BY <1>1 DEF Custody, Truth, MembersAt
  <2>0. s \in Nat /\ s < N /\ N \in Nat BY MAssumptions DEF Slots
  <2>1. CASE MembersAt(s, Truth) \cap retired' = {} BY <2>1
  <2>2. CASE \E k \in 0..N :
               /\ StablePoint(k)
               /\ s < Folded(Truth)[k].from /\ Folded(Truth)[k].from < k + LAG
               /\ \E Q \in Maj(Folded(Truth)[k].nxt) : \A q \in Q : q \notin retired' /\ s \in dlog[q]
    BY <2>2, <1>1 DEF Witness, StablePoint, Truth
  \* Otherwise r is in a member set slot s's custody rests on.
  <2>3. CASE ~(MembersAt(s, Truth) \cap retired' = {})
             /\ ~(\E k \in 0..N :
                    /\ StablePoint(k)
                    /\ s < Folded(Truth)[k].from /\ Folded(Truth)[k].from < k + LAG
                    /\ \E Q \in Maj(Folded(Truth)[k].nxt) : \A q \in Q : q \notin retired' /\ s \in dlog[q])
    <3>1. \E E \in Later(tv, s) : r \in E
      <4>1. CASE MembersAt(s, Truth) \cap retired = {}
        <5>1. r \in MembersAt(s, tv) BY <4>1, <2>3, <1>0, <1>1
        <5> QED BY <5>1, <2>0 DEF Later
      <4>2. CASE \E k \in 0..N : Witness(s, k)
        <5>1. PICK k \in 0..N : Witness(s, k) BY <4>2
        <5>2. /\ StablePoint(k) /\ s < Folded(tv)[k].from /\ Folded(tv)[k].from < k + LAG
          BY <5>1, <1>0 DEF Witness
        <5>3. PICK Q \in Maj(Folded(tv)[k].nxt) : \A q \in Q : q \notin retired /\ s \in dlog[q]
          BY <5>1, <1>0 DEF Witness
        <5>4. r \in Q BY <5>2, <5>3, <2>3, <1>0, <1>1
        <5>5. Folded(tv)[k].nxt \in Later(tv, s) BY <5>2, <2>0, WitnessEpoch
        <5> QED BY <5>4, <5>5 DEF Maj
      <4> QED BY <4>1, <4>2 DEF RInv, Custody
    <3>2. s < f
      <4> SUFFICES ASSUME f <= s PROVE FALSE BY <1>6, <2>0
      <4>1. \A E \in Later(tv, s) : E = Folded(tv)[p].nxt
        BY <1>2, <1>3, <1>4, <1>6, <2>0, TailEqual
      <4> QED BY <3>1, <4>1, <1>1, <1>3
    <3>3. \A q \in caught : s \in dlog[q]
      <4> SUFFICES ASSUME NEW q \in caught PROVE s \in dlog[q] OBVIOUS
      <4>1. q \in Replicas /\ Prefix(dlog[q]) >= f BY <1>5
      <4>2. Prefix(dlog[q]) \in 0..N /\ \A w \in 0..(Prefix(dlog[q]) - 1) : w \in dlog[q]
        BY <4>1, PrefixProps DEF RInv, Inv
      <4>3. s \in 0..(Prefix(dlog[q]) - 1) BY <4>1, <4>2, <3>2, <2>0, <1>6
      <4> QED BY <4>2, <4>3
    <3>4. caught \in Maj(st.nxt) BY <1>1 DEF Maj
    <3>5. Witness(s, p)' BY <1>0, <1>1, <1>2, <1>3, <1>6, <1>9, <3>2, <3>3, <3>4
      DEF Witness, StablePoint, Truth
    <3> QED BY <3>5, <1>2
  <2> QED BY <2>1, <2>2, <2>3, <1>1 DEF Witness, StablePoint, Truth
<1>12. Inv' BY NextInv DEF RInv, Next
<1> QED BY <1>1, <1>10, <1>11, <1>12 DEF RInv, DecidedBelow

\* Learning, persisting and crashing keep decisions and retirements; durable logs only grow.
LEMMA OtherStep ==
    ASSUME RInv, [Next]_vars, UNCHANGED <<decided, retired>>,
           \A x \in Replicas : dlog[x] \subseteq dlog'[x] /\ dlog'[x] \subseteq log'[x]
    PROVE  RInv'
<1>1. Inv' BY NextInv DEF RInv
<1>2. \A x \in retired' : \E k \in 0..N : RetiredAt(x, k)' BY DEF RInv, RetiredAt, StablePoint, Truth
<1>3. Custody'
  <2> SUFFICES ASSUME NEW s \in Slots, decided[s] # None
               PROVE  \/ MembersAt(s, Truth) \cap retired = {}
                      \/ \E k \in 0..N : Witness(s, k)'
    BY DEF Custody, Truth, MembersAt
  <2>1. CASE MembersAt(s, Truth) \cap retired = {} BY <2>1
  <2>2. CASE \E k \in 0..N : Witness(s, k)
    <3>1. PICK k \in 0..N : Witness(s, k) BY <2>2
    <3>2. PICK Q \in Maj(Folded(Truth)[k].nxt) : \A q \in Q : q \notin retired /\ s \in dlog[q]
      BY <3>1 DEF Witness
    <3>3. Q \subseteq Replicas
      <4>1. \A t \in Slots : Truth[t].m \subseteq Replicas BY TruthFacts DEF RInv
      <4>2. PICK tv : tv = Truth OBVIOUS
      <4>3. Folded(tv)[k].nxt \subseteq Replicas BY <4>1, <4>2, FoldedSubsets
      <4> QED BY <3>2, <4>2, <4>3 DEF Maj
    <3> QED BY <3>1, <3>2, <3>3 DEF Witness, StablePoint, Truth
  <2> QED BY <2>1, <2>2 DEF RInv, Custody
<1> QED BY <1>1, <1>2, <1>3 DEF RInv, DecidedBelow

THEOREM RInvNext == RInv /\ [Next]_vars => RInv'
<1> SUFFICES ASSUME RInv, [Next]_vars PROVE RInv' OBVIOUS
<1>1. ASSUME NEW r \in Replicas, NEW s \in Slots, NEW v \in Values, Decide(r, s, v) PROVE RInv'
  BY <1>1, DecideStep
<1>2. ASSUME NEW r \in Replicas, Retire(r) PROVE RInv'
  BY <1>2, RetireStep
<1>3. ASSUME NEW r \in Replicas, NEW s \in Slots, Learn(r, s) PROVE RInv'
  <2>1. UNCHANGED <<decided, retired, dlog>> /\ log' = [log EXCEPT ![r] = @ \cup {s}] BY <1>3 DEF Learn
  <2>2. \A x \in Replicas : dlog[x] \subseteq dlog'[x] /\ dlog'[x] \subseteq log'[x] BY <2>1 DEF RInv, Inv
  <2> QED BY <2>1, <2>2, OtherStep
<1>4. ASSUME NEW r \in Replicas, Persist(r) PROVE RInv'
  <2>1. UNCHANGED <<decided, retired, log>> /\ dlog' = [dlog EXCEPT ![r] = log[r]] BY <1>4 DEF Persist
  <2>2. \A x \in Replicas : dlog[x] \subseteq dlog'[x] /\ dlog'[x] \subseteq log'[x] BY <2>1 DEF RInv, Inv
  <2> QED BY <2>1, <2>2, OtherStep
<1>5. ASSUME NEW r \in Replicas, Crash(r) PROVE RInv'
  <2>1. UNCHANGED <<decided, retired, dlog>> /\ log' = [log EXCEPT ![r] = dlog[r]] BY <1>5 DEF Crash
  <2>2. \A x \in Replicas : dlog[x] \subseteq dlog'[x] /\ dlog'[x] \subseteq log'[x] BY <2>1 DEF RInv, Inv
  <2> QED BY <2>1, <2>2, OtherStep
<1>6. CASE UNCHANGED vars
  <2>1. \A x \in Replicas : dlog[x] \subseteq dlog'[x] /\ dlog'[x] \subseteq log'[x] BY <1>6 DEF vars, RInv
  <2> QED BY <1>6, <2>1, OtherStep DEF vars
<1> QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6 DEF Next

\* Retirement never loses a decided slot, even with a tolerable set of replicas down too.
THEOREM RetireSafe == Spec => []Recoverable
<1>1. Init => RInv BY RInvInit
<1>2. RInv /\ [Next]_vars => RInv' BY RInvNext
<1>3. RInv => Recoverable BY InvRecoverable
<1> QED BY <1>1, <1>2, <1>3, PTL DEF Spec
=============================================================================

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
    PROVE  LET S == {Folded(val)[k].nxt} \cup {val[t].m : t \in k..(u - 1)}
           IN  Folded(val)[u].nxt \in S /\ Eff(Folded(val)[u], u) \in S
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
=============================================================================

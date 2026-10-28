------------------------- MODULE MembershipRecover -------------------------
(***************************************************************************)
(* TLAPS proof of Membership.tla's Recoverable: retiring replicas never    *)
(* lose a decided slot, even with a tolerable set of replicas down as     *)
(* well, for any number of replicas, slots, members and changes.          *)
(*                                                                         *)
(* A decided slot is held by its own members (their registers) until one  *)
(* of them retires; then by a later epoch whose majority, none retired,   *)
(* durably holds it (Custody, pinned at a fully decided point of the log  *)
(* so later decisions never change it). Frontier tracks the latest such   *)
(* hand-over: a retiring replica's epoch takes over if it is the latest,  *)
(* or else the slots are already with the frontier's epoch.               *)
(***************************************************************************)
EXTENDS MembershipProof, FiniteSetTheorems

ASSUME RAssumptions ==
    /\ IsFiniteSet(Replicas)
    /\ InitMembers \subseteq Replicas /\ InitMembers # {}
    /\ Changes \subseteq SUBSET Replicas /\ {} \notin Changes

-----------------------------------------------------------------------------
(* Member sets *)

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

\* The latest hand-over: every retired replica is out by stable point K, and a majority of
\* the epoch in force there, none retired, durably holds every slot before it.
Frontier ==
    retired # {} =>
        \E K \in 0..N :
            /\ StablePoint(K)
            /\ Folded(Truth)[K].from <= K
            /\ \A x \in retired : \E kx \in 0..K : RetiredAt(x, kx)
            /\ \E Q \in Maj(Folded(Truth)[K].nxt) :
                   \A q \in Q : q \notin retired /\ Prefix(dlog[q]) >= Folded(Truth)[K].from

RInv ==
    /\ Inv
    /\ retired \subseteq Replicas
    /\ \A r \in Replicas : dlog[r] \subseteq log[r]
    /\ DecidedBelow
    /\ \A x \in retired : \E k \in 0..N : RetiredAt(x, k)
    /\ Frontier
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
<1> QED BY <1>1, <1>2 DEF Init, RInv, DecidedBelow, Frontier, Custody

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
<1>r. \A x \in retired, k \in 0..N : RetiredAt(x, k) => RetiredAt(x, k)'
  <2> SUFFICES ASSUME NEW x \in retired, NEW k \in 0..N, RetiredAt(x, k) PROVE RetiredAt(x, k)'
    OBVIOUS
  <2>1. RetiredAt(x, k) OBVIOUS
  <2>2. /\ \A w \in 0..(k - 1) : decided[w] # None
        /\ Folded(tv)[k].from <= k
        /\ x \notin Folded(tv)[k].nxt
        /\ \A t \in k..(N - 1) : x \notin tv[t].m
    BY <1>0, <2>1 DEF RetiredAt, StablePoint
  <2>3. Folded(tv2)[k] = Folded(tv)[k] BY <1>1, <2>2
  <2>4. \A w \in 0..(k - 1) : decided'[w] # None BY <2>2, <1>4, MAssumptions DEF Slots
  <2>5. \A t \in k..(N - 1) : x \notin tv2[t].m BY <1>1, <1>2, <2>2, MAssumptions DEF Slots
  <2> QED BY <1>a, <2>3, <2>4, <2>5, <2>2 DEF RetiredAt, StablePoint
<1>7. \A x \in retired' : \E k \in 0..N : RetiredAt(x, k)' BY <1>r, <1>2 DEF RInv
<1>f. Frontier'
  <2> SUFFICES ASSUME retired # {}
               PROVE  \E K \in 0..N :
                          /\ StablePoint(K)'
                          /\ Folded(Truth')[K].from <= K
                          /\ \A x \in retired : \E kx \in 0..K : RetiredAt(x, kx)'
                          /\ \E Q \in Maj(Folded(Truth')[K].nxt) :
                                 \A q \in Q : q \notin retired /\ Prefix(dlog[q]) >= Folded(Truth')[K].from
    BY <1>2 DEF Frontier
  <2>1. PICK K \in 0..N :
            /\ StablePoint(K)
            /\ Folded(tv)[K].from <= K
            /\ \A x \in retired : \E kx \in 0..K : RetiredAt(x, kx)
            /\ \E Q \in Maj(Folded(tv)[K].nxt) :
                   \A q \in Q : q \notin retired /\ Prefix(dlog[q]) >= Folded(tv)[K].from
    BY <1>0 DEF RInv, Frontier
  <2>2. Folded(tv2)[K] = Folded(tv)[K] BY <2>1, <1>1 DEF StablePoint
  <2>3. StablePoint(K)' BY <2>1, <1>4, MAssumptions DEF StablePoint, Slots
  <2>4. \A x \in retired : \E kx \in 0..K : RetiredAt(x, kx)'
    <3> SUFFICES ASSUME NEW x \in retired PROVE \E kx \in 0..K : RetiredAt(x, kx)' OBVIOUS
    <3>1. PICK kx \in 0..K : RetiredAt(x, kx) BY <2>1
    <3> QED BY <3>1, <1>r, MAssumptions
  <2> QED BY <2>1, <2>2, <2>3, <2>4, <1>a
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
<1> QED BY <1>5, <1>6, <1>7, <1>f, <1>8, <1>9 DEF RInv
\* A retirement hands every slot before the new epoch to a majority of it, or finds them
\* already handed on further (the frontier).
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
      /\ \A t \in Slots : decided[t] # None /\ tv[t].k = "change" /\ r \in tv[t].m => t < p
      /\ retired' = retired \cup {r} /\ UNCHANGED <<decided, log, dlog>>
  BY <1>0, <1>b, <1>c DEF Retire, Latest
<1>u. tv' = tv /\ Truth' = Truth BY <1>0, <1>1 DEF Truth
\* Nothing the custody facts read changes, but retired.
<1>s. /\ \A k : StablePoint(k)' = StablePoint(k)
      /\ \A q : dlog'[q] = dlog[q]
  BY <1>1 DEF StablePoint
<1>2. p \in 0..N /\ \A t \in 0..(p - 1) : t \in log[r] /\ decided[t] # None
  BY <1>c, PrefixProps DEF RInv, Inv, Decided
<1>3. Folded(kv)[p] = Folded(tv)[p]
  <2>1. \A t \in 0..(p - 1) : kv[t] = tv[t] BY <1>0, <1>b, <1>2, MAssumptions DEF Known, Truth, Slots
  <2> QED BY <2>1, <1>2, MAssumptions, PrefixAgree
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
\* Every slot before f is decided: a majority of the new epoch durably holds them.
<1>6. /\ f \in Nat /\ f < p + LAG /\ f <= N
      /\ \A w \in 0..(f - 1) : decided[w] # None
  <2>1. f \in Nat /\ f < p + LAG BY <1>2, MAssumptions, FromBound, FoldedShape DEF IsSt
  <2>2. PICK m \in caught : TRUE BY <1>5
  <2>3. Prefix(dlog[m]) >= f /\ m \in Replicas BY <1>5
  <2>4. Prefix(dlog[m]) \in 0..N /\ \A w \in 0..(Prefix(dlog[m]) - 1) : w \in dlog[m]
    BY <2>3, PrefixProps DEF RInv, Inv
  <2>5. \A w \in 0..(f - 1) : decided[w] # None
    BY <2>1, <2>3, <2>4, MAssumptions DEF RInv, Inv, Decided
  <2> QED BY <2>1, <2>3, <2>4, <2>5, MAssumptions
\* r's exit point: its latest epoch is in force there and no later change names r.
<1>d. PICK kr : kr = IF f <= p THEN p ELSE f OBVIOUS
<1>7. /\ kr \in 0..N /\ StablePoint(kr)
      /\ Folded(tv)[kr].nxt = st.nxt /\ Folded(tv)[kr].from = f /\ f <= kr
  <2>1. kr \in 0..N /\ f <= kr /\ p <= kr BY <1>d, <1>2, <1>6, MAssumptions
  <2>2. StablePoint(kr) BY <1>d, <1>2, <1>6, <2>1 DEF StablePoint
  <2>3. Folded(tv)[kr] = Folded(tv)[p]
    <3>1. CASE f <= p BY <3>1, <1>d
    <3>2. CASE f > p
      <4>0. kr = f BY <3>2, <1>d, <1>6, <1>2
      <4>1. Folded(tv)[p].from >= kr BY <4>0, <1>3, <1>6
      <4> QED BY <4>1, <2>1, <1>2, MAssumptions, PendingSteps
    <3> QED BY <3>1, <3>2, <1>6, <1>2
  <2> QED BY <2>1, <2>2, <2>3, <1>3
<1>8. \A t \in kr..(N - 1) : r \notin tv[t].m
  <2> SUFFICES ASSUME NEW t \in kr..(N - 1), r \in tv[t].m PROVE FALSE OBVIOUS
  <2>1. t \in Slots /\ t >= p BY <1>7, <1>d, <1>2, MAssumptions DEF Slots
  <2>2. decided[t] # None /\ tv[t].k = "change"
    <3>1. tv[t] = IF decided[t] = None THEN Write ELSE decided[t] BY <2>1, <1>0, TruthFacts DEF RInv
    <3>2. decided[t] \in Values \cup {None} BY <2>1 DEF RInv, Inv
    <3> QED BY <3>1, <3>2 DEF Write, Values, Change, None
  <2> QED BY <2>1, <2>2, <1>1, <1>2, MAssumptions
<1>9a. RetiredAt(r, kr) BY <1>0, <1>7, <1>8, <1>1 DEF RetiredAt
<1>9. RetiredAt(r, kr)' BY <1>9a, <1>u, <1>s DEF RetiredAt
\* The new frontier: r's exit point if it is the latest, the old frontier otherwise.
<1>10. \E K \in 0..N :
           /\ StablePoint(K)
           /\ Folded(tv)[K].from <= K /\ Folded(tv)[K].from >= f
           /\ \A x \in retired' : \E kx \in 0..K : RetiredAt(x, kx)'
           /\ \E Q \in Maj(Folded(tv)[K].nxt) :
                  \A q \in Q : q \notin retired' /\ Prefix(dlog[q]) >= Folded(tv)[K].from
  <2>a. \A x \in retired, k \in 0..N : RetiredAt(x, k) => RetiredAt(x, k)'
    BY <1>u, <1>1 DEF RetiredAt, StablePoint
  \* A replica out by kx <= k is in no epoch in force at k.
  <2>b. \A x \in retired, kx \in 0..N, k \in 0..N :
            RetiredAt(x, kx) /\ kx <= k => x \notin Folded(tv)[k].nxt
    <3> SUFFICES ASSUME NEW x \in retired, NEW kx \in 0..N, NEW k \in 0..N, RetiredAt(x, kx), kx <= k
                 PROVE  x \notin Folded(tv)[k].nxt
      OBVIOUS
    <3>1. Folded(tv)[kx].from <= kx /\ x \notin Folded(tv)[kx].nxt /\ \A t \in kx..(N - 1) : x \notin tv[t].m
      BY <1>0 DEF RetiredAt
    <3>2. Folded(tv)[k].nxt \in {Folded(tv)[kx].nxt} \cup {tv[t].m : t \in kx..(k - 1)}
      BY <3>1, MAssumptions, Sources
    <3> QED BY <3>1, <3>2, MAssumptions
  \* caught is a majority of st.nxt holding everything before f.
  <2>c. caught \in Maj(st.nxt) BY <1>1 DEF Maj
  \* With r's exit point as the frontier, caught is its majority.
  <2>k. ASSUME \A q \in caught : q \notin retired',
               \A x \in retired' : \E kx \in 0..kr : RetiredAt(x, kx)'
        PROVE  \E K \in 0..N :
                   /\ StablePoint(K)
                   /\ Folded(tv)[K].from <= K /\ Folded(tv)[K].from >= f
                   /\ \A x \in retired' : \E kx \in 0..K : RetiredAt(x, kx)'
                   /\ \E Q \in Maj(Folded(tv)[K].nxt) :
                          \A q \in Q : q \notin retired' /\ Prefix(dlog[q]) >= Folded(tv)[K].from
    <3>1. caught \in Maj(Folded(tv)[kr].nxt) /\ \A q \in caught : Prefix(dlog[q]) >= Folded(tv)[kr].from
      BY <2>c, <1>7
    <3> QED BY <2>k, <3>1, <1>7, <1>6
  <2>1. CASE retired = {}
    <3>1. \A q \in caught : q \notin retired' BY <2>1, <1>1
    <3>2. \A x \in retired' : \E kx \in 0..kr : RetiredAt(x, kx)' BY <2>1, <1>1, <1>9, <1>7
    <3> QED BY <3>1, <3>2, <2>k
  <2>2. CASE retired # {}
    <3>1. PICK K \in 0..N :
              /\ StablePoint(K)
              /\ Folded(tv)[K].from <= K
              /\ \A x \in retired : \E kx \in 0..K : RetiredAt(x, kx)
              /\ \E Q \in Maj(Folded(tv)[K].nxt) :
                     \A q \in Q : q \notin retired /\ Prefix(dlog[q]) >= Folded(tv)[K].from
      BY <2>2, <1>0 DEF RInv, Frontier
    <3>2. CASE K <= kr
      <4>1. \A q \in caught : q \notin retired'
        <5> SUFFICES ASSUME NEW q \in caught, q \in retired PROVE FALSE BY <1>1
        <5>1. PICK kx \in 0..K : RetiredAt(q, kx) BY <3>1
        <5>2. q \notin Folded(tv)[kr].nxt BY <5>1, <3>2, <1>7, <2>b, MAssumptions
        <5> QED BY <5>2, <1>7
      <4>2. \A x \in retired' : \E kx \in 0..kr : RetiredAt(x, kx)'
        <5> SUFFICES ASSUME NEW x \in retired' PROVE \E kx \in 0..kr : RetiredAt(x, kx)' OBVIOUS
        <5>1. CASE x = r BY <5>1, <1>9, <1>7
        <5>2. CASE x # r
          <6>1. x \in retired BY <5>2, <1>1
          <6>2. PICK kx \in 0..K : RetiredAt(x, kx) BY <6>1, <3>1
          <6> QED BY <6>1, <6>2, <2>a, <3>2, <1>7, MAssumptions
        <5> QED BY <5>1, <5>2
      <4> QED BY <4>1, <4>2, <2>k
    <3>3. CASE kr < K
      <4>1. r \notin Folded(tv)[K].nxt
        <5>1. Folded(tv)[K].nxt \in {Folded(tv)[kr].nxt} \cup {tv[t].m : t \in kr..(K - 1)}
          BY <1>7, <3>3, MAssumptions, Sources
        <5> QED BY <5>1, <1>7, <1>8, <1>1, MAssumptions
      <4>2. PICK Q \in Maj(Folded(tv)[K].nxt) :
                \A q \in Q : q \notin retired /\ Prefix(dlog[q]) >= Folded(tv)[K].from
        BY <3>1
      <4>3. \A q \in Q : q \notin retired' BY <4>1, <4>2, <1>1 DEF Maj
      <4>4. Folded(tv)[K].from >= f BY <1>7, <3>3, MAssumptions, FromMono
      <4>5. \A x \in retired' : \E kx \in 0..K : RetiredAt(x, kx)'
        <5> SUFFICES ASSUME NEW x \in retired' PROVE \E kx \in 0..K : RetiredAt(x, kx)' OBVIOUS
        <5>1. CASE x = r BY <5>1, <1>9, <1>7, <3>3
        <5>2. CASE x # r
          <6>1. x \in retired BY <5>2, <1>1
          <6>2. PICK kx \in 0..K : RetiredAt(x, kx) BY <6>1, <3>1
          <6> QED BY <6>1, <6>2, <2>a, MAssumptions
        <5> QED BY <5>1, <5>2
      <4> QED BY <3>1, <4>2, <4>3, <4>4, <4>5
    <3> QED BY <3>2, <3>3, <1>7, MAssumptions
  <2> QED BY <2>1, <2>2
<1>11. \A x \in retired' : \E k \in 0..N : RetiredAt(x, k)'
  <2> SUFFICES ASSUME NEW x \in retired' PROVE \E k \in 0..N : RetiredAt(x, k)' OBVIOUS
  <2>1. CASE x = r BY <2>1, <1>9, <1>7
  <2>2. CASE x # r
    <3>1. PICK k \in 0..N : RetiredAt(x, k) BY <2>2, <1>1 DEF RInv
    <3> QED BY <3>1, <1>u, <1>1 DEF RetiredAt, StablePoint
  <2> QED BY <2>1, <2>2
<1>12. Frontier'
  <2>1. PICK K \in 0..N :
            /\ StablePoint(K)
            /\ Folded(tv)[K].from <= K /\ Folded(tv)[K].from >= f
            /\ \A x \in retired' : \E kx \in 0..K : RetiredAt(x, kx)'
            /\ \E Q \in Maj(Folded(tv)[K].nxt) :
                   \A q \in Q : q \notin retired' /\ Prefix(dlog[q]) >= Folded(tv)[K].from
    BY <1>10
  <2>2. StablePoint(K)' BY <2>1, <1>s
  <2>3. \E Q \in Maj(Folded(Truth')[K].nxt) :
            \A q \in Q : q \notin retired' /\ Prefix(dlog'[q]) >= Folded(Truth')[K].from
    BY <2>1, <1>u, <1>s, <1>0
  <2> QED BY <2>1, <2>2, <2>3, <1>u, <1>0 DEF Frontier
<1>13. Custody'
  <2> SUFFICES ASSUME NEW s \in Slots, decided[s] # None
               PROVE  \/ MembersAt(s, Truth) \cap retired' = {}
                      \/ \E k \in 0..N : Witness(s, k)'
    BY <1>1, <1>u DEF Custody, MembersAt
  <2>0. s \in Nat /\ s < N /\ N \in Nat BY MAssumptions DEF Slots
  <2>1. CASE MembersAt(s, Truth) \cap retired' = {} BY <2>1
  <2>2. CASE \E k \in 0..N :
               /\ StablePoint(k)
               /\ s < Folded(Truth)[k].from /\ Folded(Truth)[k].from < k + LAG
               /\ \E Q \in Maj(Folded(Truth)[k].nxt) : \A q \in Q : q \notin retired' /\ s \in dlog[q]
    BY <2>2, <1>1, <1>u DEF Witness, StablePoint
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
    \* Every member set from f on leaves r out, so s comes before f.
    <3>2. s < f
      <4> SUFFICES ASSUME f <= s PROVE FALSE BY <1>6, <2>0
      <4>1. \A E \in Later(tv, s) : r \notin E BY <1>7, <1>8, <1>1, <2>0, LaterAvoid
      <4> QED BY <3>1, <4>1
    \* The frontier's epoch holds s.
    <3>3. PICK K \in 0..N :
              /\ StablePoint(K)
              /\ Folded(tv)[K].from <= K /\ Folded(tv)[K].from >= f
              /\ \E Q \in Maj(Folded(tv)[K].nxt) :
                     \A q \in Q : q \notin retired' /\ Prefix(dlog[q]) >= Folded(tv)[K].from
      BY <1>10
    <3>4. PICK Q \in Maj(Folded(tv)[K].nxt) :
              \A q \in Q : q \notin retired' /\ Prefix(dlog[q]) >= Folded(tv)[K].from
      BY <3>3
    <3>5. \A q \in Q : s \in dlog[q]
      <4> SUFFICES ASSUME NEW q \in Q PROVE s \in dlog[q] OBVIOUS
      <4>1. q \in Replicas
        <5>1. Folded(tv)[K].nxt \subseteq Replicas BY <1>5, FoldedSubsets
        <5> QED BY <5>1, <3>4 DEF Maj
      <4>2. Prefix(dlog[q]) \in 0..N /\ \A w \in 0..(Prefix(dlog[q]) - 1) : w \in dlog[q]
        BY <4>1, PrefixProps DEF RInv, Inv
      <4>3. Folded(tv)[K].from \in Nat BY FoldedShape, MAssumptions DEF IsSt
      <4>4. s \in 0..(Prefix(dlog[q]) - 1) BY <3>2, <3>3, <3>4, <4>2, <4>3, <2>0, <1>6
      <4> QED BY <4>2, <4>4
    <3>6. Folded(tv)[K].from < K + LAG BY MAssumptions, FromBound
    <3>7. Witness(s, K)'
      <4>1. StablePoint(K)' BY <3>3, <1>s
      <4>0. Folded(Truth')[K] = Folded(tv)[K] BY <1>u, <1>0
      <4>a. Folded(tv)[K].from \in Nat BY FoldedShape, MAssumptions DEF IsSt
      <4>2. s < Folded(Truth')[K].from /\ Folded(Truth')[K].from < K + LAG
        BY <4>0, <4>a, <3>2, <3>3, <3>6, <1>6, <2>0
      <4>3. \E Q2 \in Maj(Folded(Truth')[K].nxt) : \A q \in Q2 : q \notin retired' /\ s \in dlog'[q]
        BY <3>4, <3>5, <1>u, <1>0, <1>s
      <4> QED BY <4>1, <4>2, <4>3 DEF Witness
    <3> QED BY <3>7, <3>3
  <2> QED BY <2>1, <2>2, <2>3, <1>1, <1>u DEF Witness, StablePoint
<1>14. Inv' BY NextInv DEF RInv, Next
<1> QED BY <1>1, <1>11, <1>12, <1>13, <1>14 DEF RInv, DecidedBelow

\* Learning, persisting and crashing keep decisions and retirements; durable logs only grow.
LEMMA OtherStep ==
    ASSUME RInv, [Next]_vars, UNCHANGED <<decided, retired>>,
           \A x \in Replicas : dlog[x] \subseteq dlog'[x] /\ dlog'[x] \subseteq log'[x]
    PROVE  RInv'
<1>1. Inv' BY NextInv DEF RInv
<1>2. \A x \in retired' : \E k \in 0..N : RetiredAt(x, k)' BY DEF RInv, RetiredAt, StablePoint, Truth
<1>f. Frontier'
  <2> SUFFICES ASSUME retired # {}
               PROVE  \E K \in 0..N :
                          /\ StablePoint(K)
                          /\ Folded(Truth)[K].from <= K
                          /\ \A x \in retired : \E kx \in 0..K : RetiredAt(x, kx)
                          /\ \E Q \in Maj(Folded(Truth)[K].nxt) :
                                 \A q \in Q : q \notin retired /\ Prefix(dlog'[q]) >= Folded(Truth)[K].from
    BY DEF Frontier, StablePoint, RetiredAt, Truth
  <2>1. PICK K \in 0..N :
            /\ StablePoint(K)
            /\ Folded(Truth)[K].from <= K
            /\ \A x \in retired : \E kx \in 0..K : RetiredAt(x, kx)
            /\ \E Q \in Maj(Folded(Truth)[K].nxt) :
                   \A q \in Q : q \notin retired /\ Prefix(dlog[q]) >= Folded(Truth)[K].from
    BY DEF RInv, Frontier
  <2>2. PICK Q \in Maj(Folded(Truth)[K].nxt) :
            \A q \in Q : q \notin retired /\ Prefix(dlog[q]) >= Folded(Truth)[K].from
    BY <2>1
  <2>3. \A q \in Q : Prefix(dlog[q]) <= Prefix(dlog'[q])
    <3> SUFFICES ASSUME NEW q \in Q PROVE Prefix(dlog[q]) <= Prefix(dlog'[q]) OBVIOUS
    <3>1. q \in Replicas
      <4>1. \A t \in Slots : Truth[t].m \subseteq Replicas BY TruthFacts DEF RInv
      <4>2. PICK tv : tv = Truth OBVIOUS
      <4>3. Folded(tv)[K].nxt \subseteq Replicas BY <4>1, <4>2, FoldedSubsets
      <4> QED BY <2>2, <4>2, <4>3 DEF Maj
    <3>2. dlog[q] \subseteq Slots /\ dlog'[q] \subseteq Slots /\ dlog[q] \subseteq dlog'[q]
      BY <3>1, <1>1 DEF RInv, Inv
    <3> QED BY <3>2, PrefixMono
  <2>4. \A q \in Q : Prefix(dlog'[q]) >= Folded(Truth)[K].from
    <3> SUFFICES ASSUME NEW q \in Q PROVE Prefix(dlog'[q]) >= Folded(Truth)[K].from OBVIOUS
    <3>1. q \in Replicas /\ Prefix(dlog[q]) \in 0..N /\ Folded(Truth)[K].from \in Nat
      <4>1. q \in Replicas
        <5>1. \A t \in Slots : Truth[t].m \subseteq Replicas BY TruthFacts DEF RInv
        <5>2. PICK tv : tv = Truth OBVIOUS
        <5>3. Folded(tv)[K].nxt \subseteq Replicas BY <5>1, <5>2, FoldedSubsets
        <5> QED BY <2>2, <5>2, <5>3 DEF Maj
      <4>2. PICK tv : tv = Truth OBVIOUS
      <4>3. Folded(tv)[K].from \in Nat BY FoldedShape, MAssumptions DEF IsSt
      <4> QED BY <4>1, <4>2, <4>3, PrefixProps DEF RInv, Inv
    <3>2. Prefix(dlog'[q]) \in Nat
      <4>1. q \in Replicas /\ dlog'[q] \subseteq Slots BY <3>1, <1>1, <2>2 DEF RInv, Inv, Maj
      <4> QED BY <4>1, PrefixProps, MAssumptions
    <3> QED BY <2>2, <2>3, <3>1, <3>2
  <2> QED BY <2>1, <2>2, <2>4
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
<1> QED BY <1>1, <1>2, <1>f, <1>3 DEF RInv, DecidedBelow

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

\* What TLC checks is the same invariant.
THEOREM FastAgree == Recoverable <=> RecoverableFast
BY DEF Recoverable, RecoverableFast, Tolerable
=============================================================================

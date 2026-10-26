--------------------------- MODULE MembershipProof ---------------------------
(***************************************************************************)
(* TLAPS proof of Membership.tla's ViewAgree (the window rule): every     *)
(* replica that may drive a slot computes its true members, for any        *)
(* number of replicas, slots and members and any LAG >= 1.                 *)
(*                                                                         *)
(* The only state invariant is that logs hold decided slots and decisions  *)
(* never change; the rest is about the schedule fold alone: views that     *)
(* agree on a prefix fold to the same schedule, and a slot within LAG      *)
(* before s, change or not, does not change who the members of s are.     *)
(***************************************************************************)
EXTENDS Membership, TLAPS, NaturalsInduction

ASSUME MAssumptions == N \in Nat /\ LAG \in Nat \ {0}

\* The members of slot s under schedule st (MembersAt is this at the folded schedule).
Eff(st, s) == IF st.from <= s THEN st.nxt ELSE st.cur

IsSt(st) == st = [cur |-> st.cur, nxt |-> st.nxt, from |-> st.from] /\ st.from \in Nat

Decided == {t \in Slots : decided[t] # None}

\* Logs hold only decided slots.
Inv ==
    /\ decided \in [Slots -> Values \cup {None}]
    /\ log \in [Replicas -> SUBSET Slots]
    /\ dlog \in [Replicas -> SUBSET Slots]
    /\ \A r \in Replicas : log[r] \subseteq Decided /\ dlog[r] \subseteq Decided

-----------------------------------------------------------------------------
(* The schedule fold *)

LEMMA FoldedDef ==
    ASSUME NEW val
    PROVE  /\ Folded(val)[0] = Start
           /\ \A n \in Nat : Folded(val)[n + 1] = FoldStep(Folded(val)[n], n, val)
<1> DEFINE Def(v, i) == FoldStep(v, i - 1, val)
<1>1. NatInductiveDefHypothesis(Folded(val), Start, Def)
  BY DEF Folded, NatInductiveDefHypothesis
<1>2. NatInductiveDefConclusion(Folded(val), Start, Def) BY <1>1, NatInductiveDef
<1>3. Folded(val) = [i \in Nat |-> IF i = 0 THEN Start ELSE FoldStep(Folded(val)[i - 1], i - 1, val)]
  BY <1>2 DEF NatInductiveDefConclusion
<1> QED BY <1>3

LEMMA StepShape ==
    ASSUME NEW st, IsSt(st), NEW t \in Nat, NEW val
    PROVE  IsSt(FoldStep(st, t, val))
BY MAssumptions DEF IsSt, FoldStep

LEMMA FoldedShape ==
    ASSUME NEW val
    PROVE  \A n \in Nat : IsSt(Folded(val)[n])
<1> DEFINE P(n) == IsSt(Folded(val)[n])
<1>1. P(0) BY FoldedDef DEF IsSt, Start
<1>2. \A n \in Nat : P(n) => P(n + 1) BY FoldedDef, StepShape
<1> HIDE DEF P
<1>3. \A n \in Nat : P(n) BY <1>1, <1>2, NatInduction
<1> QED BY <1>3 DEF P

\* Views that agree on slots 0..k-1 fold to the same schedule at k.
LEMMA PrefixAgree ==
    ASSUME NEW v1, NEW v2, NEW k \in Nat, \A t \in 0..(k - 1) : v1[t] = v2[t]
    PROVE  Folded(v1)[k] = Folded(v2)[k]
<1> DEFINE P(n) == n <= k => Folded(v1)[n] = Folded(v2)[n]
<1>1. P(0) BY FoldedDef
<1>2. \A n \in Nat : P(n) => P(n + 1)
  <2> SUFFICES ASSUME NEW n \in Nat, P(n), n + 1 <= k PROVE Folded(v1)[n + 1] = Folded(v2)[n + 1]
    OBVIOUS
  <2>1. Folded(v1)[n] = Folded(v2)[n] /\ v1[n] = v2[n] OBVIOUS
  <2> QED BY <2>1, FoldedDef DEF FoldStep
<1> HIDE DEF P
<1>3. \A n \in Nat : P(n) BY <1>1, <1>2, NatInduction
<1> QED BY <1>3 DEF P

\* A slot t with t < s < t + LAG does not change the members of s, whatever it holds.
LEMMA StepEff ==
    ASSUME NEW st, IsSt(st), NEW t \in Nat, NEW s \in Nat, t < s, s < t + LAG, NEW val
    PROVE  Eff(FoldStep(st, t, val), s) = Eff(st, s)
<1>1. CASE st.from <= t
  <2> DEFINE act == [st EXCEPT !.cur = st.nxt]
  <2>1. act.from = st.from /\ act.nxt = st.nxt /\ act.cur = st.nxt BY DEF IsSt
  <2>2. Eff(st, s) = st.nxt BY <1>1 DEF Eff, IsSt
  <2>3. CASE val[t].k = "change"
    <3>1. FoldStep(st, t, val) = [act EXCEPT !.nxt = val[t].m, !.from = t + LAG]
      BY <1>1, <2>1, <2>3 DEF FoldStep
    <3>2. FoldStep(st, t, val).from = t + LAG /\ FoldStep(st, t, val).cur = st.nxt
      BY <3>1, <2>1 DEF IsSt
    <3>3. ~(t + LAG <= s) BY MAssumptions
    <3> QED BY <3>2, <3>3, <2>2 DEF Eff
  <2>4. CASE val[t].k # "change"
    <3>1. FoldStep(st, t, val) = act BY <1>1, <2>4 DEF FoldStep
    <3> QED BY <3>1, <2>1, <2>2, <1>1 DEF Eff, IsSt
  <2> QED BY <2>3, <2>4
<1>2. CASE ~(st.from <= t)
  <2>1. FoldStep(st, t, val) = st BY <1>2 DEF FoldStep
  <2> QED BY <2>1
<1> QED BY <1>1, <1>2

\* Folding the slots k..s-1, all within LAG before s, keeps the members of s.
LEMMA WindowEff ==
    ASSUME NEW val, NEW k \in Nat, NEW s \in Nat, k <= s, s < k + LAG
    PROVE  Eff(Folded(val)[s], s) = Eff(Folded(val)[k], s)
<1> DEFINE P(j) == k + j <= s => Eff(Folded(val)[k + j], s) = Eff(Folded(val)[k], s)
<1>1. P(0) OBVIOUS
<1>2. \A j \in Nat : P(j) => P(j + 1)
  <2> SUFFICES ASSUME NEW j \in Nat, P(j), k + j + 1 <= s
               PROVE  Eff(Folded(val)[k + j + 1], s) = Eff(Folded(val)[k], s)
    OBVIOUS
  <2>1. Folded(val)[k + j + 1] = FoldStep(Folded(val)[k + j], k + j, val) BY FoldedDef
  <2>2. IsSt(Folded(val)[k + j]) BY FoldedShape
  <2>3. k + j < s /\ s < (k + j) + LAG BY MAssumptions
  <2> QED BY <2>1, <2>2, <2>3, StepEff
<1> HIDE DEF P
<1>3. \A j \in Nat : P(j) BY <1>1, <1>2, NatInduction
<1>4. P(s - k) BY <1>3
<1> QED BY <1>4 DEF P

-----------------------------------------------------------------------------
(* The commit index *)

LEMMA PrefixProps ==
    ASSUME NEW L \in SUBSET Slots
    PROVE  /\ Prefix(L) \in 0..N
           /\ \A t \in 0..(Prefix(L) - 1) : t \in L
<1> DEFINE Stop(n) == n \in 0..N /\ (n = N \/ n \notin L)
<1>1. Stop(N) BY MAssumptions
<1>2. \E m \in Nat : Stop(m) /\ \A j \in 0..(m - 1) : ~Stop(j)
  <2> HIDE DEF Stop
  <2> QED BY <1>1, MAssumptions, SmallestNatural
<1>3. PICK m \in Nat : Stop(m) /\ \A j \in 0..(m - 1) : ~Stop(j) BY <1>2
<1>4. \A t \in 0..(m - 1) : t \in L BY <1>3, MAssumptions
<1>5. \E n \in 0..N : (\A t \in 0..n-1 : t \in L) /\ (n = N \/ n \notin L) BY <1>3, <1>4
<1>6. /\ Prefix(L) \in 0..N
      /\ \A t \in 0..Prefix(L) - 1 : t \in L
  BY <1>5 DEF Prefix
<1> QED BY <1>6

-----------------------------------------------------------------------------
(* ViewAgree *)

THEOREM InvAgree == Inv => ViewAgree
<1> SUFFICES ASSUME Inv, NEW r \in Replicas, NEW s \in Slots, CanDrive(r, s)
             PROVE  MembersAt(s, Known(r)) = MembersAt(s, Truth)
  BY DEF ViewAgree
<1> DEFINE p == Prefix(log[r])
           k == IF s <= p THEN s ELSE p
<1>1. p \in 0..N /\ \A t \in 0..(p - 1) : t \in log[r] BY PrefixProps DEF Inv
<1>2. s \in Nat /\ s < p + LAG BY MAssumptions DEF CanDrive, Slots
<1>3. k \in Nat /\ k <= s /\ s < k + LAG /\ k <= p BY <1>1, <1>2, MAssumptions
\* Below the commit index the replica's view is the truth.
<1>4. \A t \in 0..(k - 1) : Known(r)[t] = Truth[t]
  <2> SUFFICES ASSUME NEW t \in 0..(k - 1) PROVE Known(r)[t] = Truth[t] OBVIOUS
  <2>1. t \in log[r] /\ t \in Slots BY <1>1, <1>3, MAssumptions DEF Slots
  <2>2. decided[t] # None BY <2>1 DEF Inv, Decided
  <2> QED BY <2>1, <2>2 DEF Known, Truth
\* The fold lemmas take constants: name the two views.
<1>a. PICK kv : kv = Known(r) OBVIOUS
<1>b. PICK tv : tv = Truth OBVIOUS
<1>c. PICK kk \in Nat : kk = k BY <1>3
<1>5. Folded(kv)[kk] = Folded(tv)[kk] BY <1>3, <1>4, <1>a, <1>b, <1>c, PrefixAgree
<1>d. s \in Nat /\ kk <= s /\ s < kk + LAG BY <1>2, <1>3, <1>c
<1>6. Eff(Folded(kv)[s], s) = Eff(Folded(kv)[kk], s) BY <1>d, WindowEff
<1>7. Eff(Folded(tv)[s], s) = Eff(Folded(tv)[kk], s) BY <1>d, WindowEff
<1> QED BY <1>5, <1>6, <1>7, <1>a, <1>b DEF MembersAt, Eff

-----------------------------------------------------------------------------
(* Inv is inductive *)

LEMMA InitInv == Init => Inv
BY DEF Init, Inv, Decided

LEMMA NextInv == Inv /\ [Next]_vars => Inv'
<1> SUFFICES ASSUME Inv, [Next]_vars PROVE Inv' OBVIOUS
<1>0. \A v \in Values : v # None BY DEF Values, Write, Change, None
<1>1. ASSUME NEW r \in Replicas, NEW s \in Slots, NEW v \in Values, Decide(r, s, v) PROVE Inv'
  <2>1. decided' = [decided EXCEPT ![s] = v] /\ log' = [log EXCEPT ![r] = @ \cup {s}]
        /\ dlog' = dlog /\ decided[s] = None
    BY <1>1 DEF Decide, CanDrive
  <2>2. Decided \subseteq Decided' /\ s \in Decided' BY <2>1, <1>0 DEF Inv, Decided
  <2> QED BY <2>1, <2>2, <1>0 DEF Inv
<1>2. ASSUME NEW r \in Replicas, NEW s \in Slots, Learn(r, s) PROVE Inv'
  BY <1>2 DEF Learn, Inv, Decided
<1>3. ASSUME NEW r \in Replicas, Persist(r) PROVE Inv'
  BY <1>3 DEF Persist, Inv, Decided
<1>4. ASSUME NEW r \in Replicas, Crash(r) PROVE Inv'
  BY <1>4 DEF Crash, Inv, Decided
<1>5. ASSUME NEW r \in Replicas, Retire(r) PROVE Inv'
  BY <1>5 DEF Retire, Inv, Decided
<1>6. CASE UNCHANGED vars BY <1>6 DEF vars, Inv, Decided
<1> QED BY <1>1, <1>2, <1>3, <1>4, <1>5, <1>6 DEF Next

THEOREM WindowRule == Spec => []ViewAgree
<1>1. Init => Inv BY InitInv
<1>2. Inv /\ [Next]_vars => Inv' BY NextInv
<1>3. Inv => ViewAgree BY InvAgree
<1> QED BY <1>1, <1>2, <1>3, PTL DEF Spec
=============================================================================

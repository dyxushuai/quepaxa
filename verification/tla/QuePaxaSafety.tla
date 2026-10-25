--------------------------- MODULE QuePaxaSafety ---------------------------
(***************************************************************************)
(* TLAPS proof that QuePaxa.tla never decides two values: IndInv          *)
(* (QuePaxaProof.tla) holds in every state and implies Agreement, for any *)
(* number of recorders and proposers and any step bound.                  *)
(*                                                                         *)
(* Check with tlapm 1.6: verification/tla/prove.sh                         *)
(***************************************************************************)
EXTENDS QuePaxaProof, TLAPS, NaturalsInduction, FiniteSetTheorems

\* Finitely many recorders; proposers numbered from 1; drawn priorities below H.
ASSUME Assumptions ==
    /\ IsFiniteSet(Recorders) /\ Recorders # {}
    /\ \E n \in Nat : Proposers \subseteq 1..n
    /\ Leader \in Proposers
    /\ Prios \subseteq 1..(H - 1)
    /\ MaxStep \in Nat

-----------------------------------------------------------------------------
(* The order on proposals *)

LEMMA ProposalFields ==
    ASSUME NEW a \in Proposal
    PROVE  /\ a.prio \in 1..H
           /\ a.key \in Nat \ {0}
           /\ a = [prio |-> a.prio, key |-> a.key]
           /\ a # Nil
<1>1. a.prio \in 1..H BY Assumptions DEF Proposal, H
<1>2. a.key \in Nat \ {0} BY Assumptions DEF Proposal
<1>3. a = [prio |-> a.prio, key |-> a.key] BY DEF Proposal
<1>4. a # Nil BY <1>1 DEF Nil
<1> QED BY <1>1, <1>2, <1>3, <1>4

LEMMA Order ==
    ASSUME NEW a \in Proposal, NEW b \in Proposal, NEW c \in Proposal
    PROVE  /\ Leq(a, a)
           /\ Leq(a, b) \/ Leq(b, a)
           /\ Leq(a, b) /\ Leq(b, c) => Leq(a, c)
           /\ Leq(a, b) /\ Leq(b, a) => a = b
<1> USE ProposalFields
<1>1. Leq(a, a) BY DEF Leq, Gt
<1>2. Leq(a, b) \/ Leq(b, a) BY DEF Leq, Gt
<1>3. Leq(a, b) /\ Leq(b, c) => Leq(a, c) BY DEF Leq, Gt
<1>4. Leq(a, b) /\ Leq(b, a) => a = b
  <2> SUFFICES ASSUME Leq(a, b), Leq(b, a) PROVE a = b OBVIOUS
  <2>1. a.prio = b.prio /\ a.key = b.key BY DEF Leq, Gt
  <2> QED BY <2>1
<1> QED BY <1>1, <1>2, <1>3, <1>4

\* A nonempty set of naturals with an upper bound has a largest element.
LEMMA NatMax ==
    ASSUME NEW N \in Nat, NEW T \in SUBSET (0..N), T # {}
    PROVE  \E x \in T : \A y \in T : y <= x
<1> DEFINE Bound(n) == \A y \in T : y <= n
<1>1. Bound(N) OBVIOUS
<1>2. \E m \in Nat : Bound(m) /\ \A k \in 0..m-1 : ~Bound(k)
  <2> HIDE DEF Bound
  <2> QED BY <1>1, SmallestNatural
<1>3. PICK m \in Nat : Bound(m) /\ \A k \in 0..m-1 : ~Bound(k) BY <1>2
<1>4. m \in T
  <2> SUFFICES ASSUME m \notin T PROVE FALSE OBVIOUS
  <2>1. PICK y \in T : TRUE OBVIOUS
  <2>2. m # 0 BY <1>3, <2>1
  <2>3. Bound(m - 1) BY <1>3, <2>2
  <2>4. m - 1 \in 0..m-1 BY <2>2
  <2> QED BY <1>3, <2>3, <2>4
<1> QED BY <1>3, <1>4

LEMMA MaxProps ==
    ASSUME NEW S \in SUBSET Proposal, S # {}
    PROVE  /\ Max(S) \in S
           /\ \A o \in S : Leq(o, Max(S))
<1> USE ProposalFields
<1>1. PICK kn \in Nat : Proposers \subseteq 1..kn BY Assumptions
<1>2. PICK mp \in {w.prio : w \in S} : \A y \in {w.prio : w \in S} : y <= mp
  <2>1. {w.prio : w \in S} \in SUBSET (0..H) BY DEF H
  <2> QED BY <2>1, NatMax, H \in Nat DEF H
<1> DEFINE S1 == {w \in S : w.prio = mp}
<1>3. S1 # {} BY <1>2
<1>4. PICK mk \in {w.key : w \in S1} : \A y \in {w.key : w \in S1} : y <= mk
  <2>1. {w.key : w \in S1} \in SUBSET (0..kn) BY <1>1 DEF Proposal
  <2> QED BY <2>1, <1>3, NatMax
<1>5. PICK m \in S1 : m.key = mk BY <1>4
<1>6. \A o \in S : Leq(o, m) BY <1>2, <1>4, <1>5 DEF Leq, Gt
<1>7. \E x \in S : \A o \in S : ~Gt(o, x) BY <1>5, <1>6 DEF Leq
<1> QED BY <1>7 DEF Max, Leq
-----------------------------------------------------------------------------
(* Agreement follows from IndInv *)

LEMMA QuorumsIntersect ==
    ASSUME NEW Q1 \in Quorums, NEW Q2 \in Quorums
    PROVE  Q1 \cap Q2 # {}
<1>1. Cardinality(Recorders) \in Nat BY Assumptions, FS_CardinalityType
<1>2. /\ Cardinality(Q1) \in Nat /\ Cardinality(Q1) >= Cardinality(Recorders) \div 2 + 1
      /\ Cardinality(Q2) \in Nat /\ Cardinality(Q2) >= Cardinality(Recorders) \div 2 + 1
  BY Assumptions, FS_Subset, FS_CardinalityType DEF Quorums, Quorum
<1>3. Cardinality(Q1) + Cardinality(Q2) > Cardinality(Recorders)
  BY <1>1, <1>2
<1> QED BY <1>3, Assumptions, FS_MajoritiesIntersect DEF Quorums

\* What a recorder's history says about a step it entered.
LEMMA EntryFacts ==
    ASSUME IndInv, NEW x \in Recorders, NEW t, Entered(x, t)
    PROVE  /\ EntryOf(x, t) \in seen
           /\ EntryOf(x, t).r = x /\ EntryOf(x, t).step = t
           /\ t \in Nat
           /\ FirstAt(x, t) \in RecAt(x, t)
           /\ PrevAt(x, t) = MaxOf(RecAt(x, t - 1))
<1>1. EntryOf(x, t) \in SeenAt(x, t) BY DEF Entered, EntryOf
<1> QED BY <1>1 DEF IndInv, SeenInv, TypeInv, SeenAt, FirstAt, PrevAt

\* Recorded proposals are proposals some request carried at that step.
LEMMA RecProps ==
    ASSUME IndInv, NEW x, NEW t
    PROVE  /\ RecAt(x, t) \subseteq Proposal
           /\ RecAt(x, t) \subseteq ReqProps(t)
<1>1. RecAt(x, t) \subseteq Proposal BY DEF IndInv, TypeInv, RecAt
<1>2. RecAt(x, t) \subseteq ReqProps(t)
  <2> SUFFICES ASSUME NEW w \in RecAt(x, t) PROVE w \in ReqProps(t) OBVIOUS
  <2>1. PICK e \in rec : e.r = x /\ e.step = t /\ e.prop = w BY DEF RecAt
  <2>2. PICK m \in Reqs : m.to = e.r /\ m.step = e.step /\ m.prop = e.prop
    BY <2>1 DEF IndInv, SeenInv
  <2> QED BY <2>1, <2>2 DEF ReqProps
<1> QED BY <1>1, <1>2

\* A step's `prev` is Nil exactly when nothing was recorded the step before, and else
\* the best recorded there.
LEMMA PrevFacts ==
    ASSUME IndInv, NEW x \in Recorders, NEW t, Entered(x, t)
    PROVE  /\ PrevAt(x, t) = Nil <=> RecAt(x, t - 1) = {}
           /\ RecAt(x, t - 1) # {} =>
                  /\ PrevAt(x, t) = Max(RecAt(x, t - 1))
                  /\ PrevAt(x, t) \in RecAt(x, t - 1)
                  /\ PrevAt(x, t) \in Proposal
                  /\ \A u \in RecAt(x, t - 1) : Leq(u, PrevAt(x, t))
<1>1. PrevAt(x, t) = MaxOf(RecAt(x, t - 1)) BY EntryFacts
<1>2. RecAt(x, t - 1) \subseteq Proposal BY RecProps
<1>3. RecAt(x, t - 1) # {} =>
          /\ Max(RecAt(x, t - 1)) \in RecAt(x, t - 1)
          /\ \A u \in RecAt(x, t - 1) : Leq(u, Max(RecAt(x, t - 1)))
  BY <1>2, MaxProps
<1>4. RecAt(x, t - 1) # {} => Max(RecAt(x, t - 1)) # Nil BY <1>2, <1>3, ProposalFields
<1> QED BY <1>1, <1>2, <1>3, <1>4 DEF MaxOf

LEMMA ReqTyped ==
    ASSUME IndInv, NEW m \in Reqs
    PROVE  /\ m.prop \in Proposal /\ m.step \in Nat /\ m.from \in Proposers /\ m.to \in Recorders
<1>1. m \in msgs /\ m.type = "req" BY DEF Reqs
<1>2. m \in ReqMsg \cup RepMsg BY <1>1 DEF IndInv, TypeInv
<1>3. m \notin RepMsg BY <1>1 DEF RepMsg
<1> QED BY <1>2, <1>3 DEF ReqMsg

LEMMA ReqPropsTyped ==
    ASSUME IndInv, NEW t, NEW w \in ReqProps(t)
    PROVE  \E m \in Reqs : m.step = t /\ m.prop = w /\ w \in Proposal
BY ReqTyped DEF ReqProps

\* Once every request at step t0 carries key k, so does every request after it.
LEMMA Propagate ==
    ASSUME IndInv, NEW k, NEW t0 \in Nat, t0 >= 5, t0 % 4 \in {0, 1},
           \A m \in Reqs : m.step = t0 => m.prop.key = k
    PROVE  \A m \in Reqs : m.step >= t0 => m.prop.key = k
\* Induction needs a constant-level predicate: R stands for the current requests.
<1>0. PICK R : R = Reqs OBVIOUS
<1> DEFINE KeyAt(n) == \A m \in R : m.step = t0 + n => m.prop.key = k
<1>1. \A n \in Nat : (\A j \in 0..(n-1) : KeyAt(j)) => KeyAt(n)
  <2> SUFFICES ASSUME NEW n \in Nat, \A j \in 0..(n-1) : KeyAt(j),
                      NEW m \in Reqs, m.step = t0 + n
               PROVE  m.prop.key = k
    BY <1>0
  <2> DEFINE t == t0 + n
  \* A proposal at step s in [t0, t) has key k.
  <2>1. \A s \in t0..(t-1) : \A w \in ReqProps(s) : w.key = k
    <3> SUFFICES ASSUME NEW s \in t0..(t-1), NEW w \in ReqProps(s) PROVE w.key = k OBVIOUS
    <3>1. PICK m2 \in Reqs : m2.step = s /\ m2.prop = w BY ReqPropsTyped
    <3>2. s - t0 \in 0..(n-1) OBVIOUS
    <3> QED BY <3>1, <3>2, <1>0
  <2>2. CASE n = 0
    <3>1. m.step = t0 BY <2>2
    <3> QED BY <3>1
  <2>3. CASE n # 0 /\ t % 4 = 1
    <3>1. PICK Q \in Quorums : /\ \A y \in Q : Entered(y, t - 1)
                               /\ \E y \in Q : FirstAt(y, t - 1) = m.prop
      BY <2>3 DEF IndInv, SupportInv
    <3>2. PICK y \in Q : FirstAt(y, t - 1) = m.prop BY <3>1
    <3>3. m.prop \in ReqProps(t - 1)
      BY <3>1, <3>2, EntryFacts, RecProps DEF Quorums
    <3> QED BY <3>3, <2>1, <2>3
  <2>4. CASE n # 0 /\ t % 4 \in {2, 3}
    <3>1. m.prop \in ReqProps(t - 1) BY <2>4 DEF IndInv, SupportInv
    <3> QED BY <3>1, <2>1, <2>4
  <2>5. CASE n # 0 /\ t % 4 = 0
    <3>1. t - 2 >= t0 /\ t > FirstStep BY <2>5 DEF FirstStep
    <3>2. PICK Q \in Quorums :
              /\ \A y \in Q : Entered(y, t - 1)
              /\ LET P == {PrevAt(y, t - 1) : y \in Q} \ {Nil}
                 IN IF P # {} THEN m.prop.key = Max(P).key
                    ELSE \E w \in ReqProps(t - 1) : w.key = m.prop.key
      BY <2>5, <3>1 DEF IndInv, SupportInv
    <3> DEFINE P == {PrevAt(y, t - 1) : y \in Q} \ {Nil}
    <3>3. CASE P # {}
      <4>0. (t - 1) - 1 = t - 2 OBVIOUS
      \* A `prev` at t - 1 other than Nil is a proposal recorded at t - 2.
      <4>1. \A y \in Q : PrevAt(y, t - 1) # Nil =>
                PrevAt(y, t - 1) \in Proposal /\ PrevAt(y, t - 1) \in ReqProps(t - 2)
        <5> SUFFICES ASSUME NEW y \in Q, PrevAt(y, t - 1) # Nil
                     PROVE PrevAt(y, t - 1) \in Proposal /\ PrevAt(y, t - 1) \in ReqProps(t - 2)
          OBVIOUS
        <5>1. y \in Recorders /\ Entered(y, t - 1) BY <3>2 DEF Quorums
        <5>2. RecAt(y, t - 2) # {} BY <5>1, <4>0, PrevFacts
        <5>3. PrevAt(y, t - 1) \in RecAt(y, t - 2) /\ PrevAt(y, t - 1) \in Proposal
          BY <5>1, <5>2, <4>0, PrevFacts
        <5> QED BY <5>3, RecProps
      <4>2. P \subseteq Proposal BY <4>1
      <4>3. Max(P) \in P BY <3>3, <4>2, MaxProps
      <4>5. Max(P) \in ReqProps(t - 2) BY <4>1, <4>3
      <4> QED BY <3>2, <3>3, <4>5, <2>1, <3>1
    <3>4. CASE P = {}
      <4>1. PICK w \in ReqProps(t - 1) : w.key = m.prop.key BY <3>2, <3>4
      <4> QED BY <4>1, <2>1, <3>1
    <3> QED BY <3>3, <3>4
  <2> QED BY <2>2, <2>3, <2>4, <2>5
\* Strong induction, as plain induction on "every step up to n".
<1> DEFINE Upto(n) == \A j \in 0..n : KeyAt(j)
<1>a. Upto(0) BY <1>1
<1>b. \A n \in Nat : Upto(n) => Upto(n + 1)
  <2> SUFFICES ASSUME NEW n \in Nat, Upto(n) PROVE Upto(n + 1) OBVIOUS
  <2>1. KeyAt(n + 1) BY <1>1
  <2> QED BY <2>1
<1> HIDE DEF Upto, KeyAt
<1>c. \A n \in Nat : Upto(n) BY <1>a, <1>b, NatInduction
<1>2. \A n \in Nat : KeyAt(n) BY <1>c DEF Upto
<1> SUFFICES ASSUME NEW m \in Reqs, m.step >= t0 PROVE m.prop.key = k OBVIOUS
<1>3. m.step \in Nat BY ReqTyped
<1>4. m.step - t0 \in Nat /\ m.step = t0 + (m.step - t0) BY <1>3
<1>5. KeyAt(m.step - t0) BY <1>2, <1>4
<1> QED BY <1>0, <1>4, <1>5 DEF KeyAt

\* q decided at phase 2: its quorum's best `prev` bounds every proposal of that step.
LEMMA Phase2Bound ==
    ASSUME IndInv, NEW q \in Proposers, pst[q].decided # Nil, pst[q].step % 4 = 2
    PROVE  \A m \in Reqs : m.step = pst[q].step => Leq(m.prop, pst[q].p)
<1> DEFINE d == pst[q].step  v == pst[q].p
<1>1. /\ GotQ(q) \in Quorums
      /\ \A r \in Got(q) : r.prev # Nil => Leq(r.prev, v)
  BY DEF IndInv, DecInv, FirstStep
<1> SUFFICES ASSUME NEW m \in Reqs, m.step = d PROVE Leq(m.prop, v) OBVIOUS
<1>2. PICK Q1 \in Quorums : \A y \in Q1 : \E u \in RecAt(y, d - 1) : Leq(m.prop, u)
  BY DEF IndInv, SupportInv
<1>3. PICK y \in Q1 \cap GotQ(q) : TRUE BY <1>1, QuorumsIntersect
<1>4. PICK u \in RecAt(y, d - 1) : Leq(m.prop, u) BY <1>2, <1>3
<1>5. PICK r \in pst[q].replies[y] : TRUE BY <1>3 DEF GotQ
<1>6. /\ r \in Got(q) /\ Entered(y, d) /\ PrevAt(y, d) = r.prev
  BY <1>3, <1>5 DEF IndInv, GotInv, Got, GotQ, Quorums
<1>7. /\ PrevAt(y, d) \in Proposal /\ Leq(u, PrevAt(y, d))
  <2>1. y \in Recorders BY <1>3 DEF Quorums
  <2>2. RecAt(y, d - 1) # {} BY <1>4
  <2> QED BY <1>4, <1>6, <2>1, <2>2, PrevFacts
<1>8. Leq(r.prev, v) BY <1>1, <1>6, <1>7, ProposalFields
<1>9. /\ m.prop \in Proposal /\ u \in Proposal /\ v \in Proposal
  BY <1>4, ReqTyped, RecProps DEF IndInv, TypeInv
<1> QED BY <1>4, <1>6, <1>7, <1>8, <1>9, Order

\* q decided v at phase 2 of a round: from the next round on, every proposal has v's key.
LEMMA Phase2Lock ==
    ASSUME IndInv, NEW q \in Proposers, pst[q].decided # Nil, pst[q].step % 4 = 2
    PROVE  \A m \in Reqs : m.step >= pst[q].step + 2 => m.prop.key = pst[q].p.key
<1> DEFINE d == pst[q].step  v == pst[q].p
<1>1. /\ GotQ(q) \in Quorums /\ d \in Nat /\ d >= FirstStep /\ v \in Proposal
      /\ pst[q].sent = [x \in Recorders |-> v]
  BY DEF IndInv, DecInv, SentInv, TypeInv, Drawing, FirstStep
<1>2. d + 2 >= 5 /\ (d + 2) % 4 = 0 /\ d + 2 > FirstStep BY <1>1 DEF FirstStep
<1>3. \A m \in Reqs : m.step = d + 2 => m.prop.key = v.key
  <2> SUFFICES ASSUME NEW m \in Reqs, m.step = d + 2 PROVE m.prop.key = v.key OBVIOUS
  <2>b. m.step % 4 = 0 /\ m.step > FirstStep /\ m.step - 1 = d + 1 BY <1>1, <1>2
  <2>c. \E Q \in Quorums :
            /\ \A y \in Q : Entered(y, m.step - 1)
            /\ LET P == {PrevAt(y, m.step - 1) : y \in Q} \ {Nil}
               IN IF P # {} THEN m.prop.key = Max(P).key
                  ELSE \E w \in ReqProps(m.step - 1) : w.key = m.prop.key
    BY <2>b DEF IndInv, SupportInv
  <2>1. PICK Q \in Quorums :
            /\ \A y \in Q : Entered(y, d + 1)
            /\ LET P == {PrevAt(y, d + 1) : y \in Q} \ {Nil}
               IN IF P # {} THEN m.prop.key = Max(P).key
                  ELSE \E w \in ReqProps(d + 1) : w.key = m.prop.key
    BY <2>b, <2>c
  <2> DEFINE P == {PrevAt(y, d + 1) : y \in Q} \ {Nil}
  \* A member of q's quorum recorded v at d, so its `prev` at d + 1 covers v.
  <2>2. PICK x \in Q \cap GotQ(q) : TRUE BY <1>1, QuorumsIntersect
  <2>3. v \in RecAt(x, d)
    <3>1. PICK r \in pst[q].replies[x] : TRUE BY <2>2 DEF GotQ
    <3> QED BY <1>1, <2>2, <3>1 DEF IndInv, GotInv, Quorums
  <2>0. (d + 1) - 1 = d BY <1>1
  \* A `prev` at d + 1 other than Nil is a proposal recorded at d.
  <2>a. \A y \in Q : PrevAt(y, d + 1) # Nil =>
            /\ PrevAt(y, d + 1) \in Proposal /\ PrevAt(y, d + 1) \in ReqProps(d)
            /\ \A u \in RecAt(y, d) : Leq(u, PrevAt(y, d + 1))
    <3> SUFFICES ASSUME NEW y \in Q, PrevAt(y, d + 1) # Nil
                 PROVE  /\ PrevAt(y, d + 1) \in Proposal /\ PrevAt(y, d + 1) \in ReqProps(d)
                        /\ \A u \in RecAt(y, d) : Leq(u, PrevAt(y, d + 1))
      OBVIOUS
    <3>1. y \in Recorders /\ Entered(y, d + 1) BY <2>1 DEF Quorums
    <3>2. RecAt(y, d) # {} BY <3>1, <2>0, PrevFacts
    <3>3. /\ PrevAt(y, d + 1) \in RecAt(y, d) /\ PrevAt(y, d + 1) \in Proposal
          /\ \A u \in RecAt(y, d) : Leq(u, PrevAt(y, d + 1))
      BY <3>1, <3>2, <2>0, PrevFacts
    <3> QED BY <3>3, RecProps
  <2>4. /\ PrevAt(x, d + 1) \in Proposal /\ Leq(v, PrevAt(x, d + 1))
        /\ PrevAt(x, d + 1) \in P
    <3>1. x \in Recorders /\ Entered(x, d + 1) BY <2>1, <2>2 DEF Quorums
    <3>2. PrevAt(x, d + 1) # Nil BY <3>1, <2>0, <2>3, PrevFacts
    <3> QED BY <2>a, <2>2, <2>3, <3>2
  <2>5. P \subseteq Proposal BY <2>a
  <2>6. /\ Max(P) \in P /\ Leq(PrevAt(x, d + 1), Max(P)) BY <2>4, <2>5, MaxProps
  \* Every `prev` at d + 1 is a proposal of step d, so at most v.
  <2>7. Leq(Max(P), v)
    <3>3. Max(P) \in ReqProps(d) BY <2>a, <2>6
    <3>4. PICK m2 \in Reqs : m2.step = d /\ m2.prop = Max(P) BY <3>3, ReqPropsTyped
    <3> QED BY <3>4, Phase2Bound
  <2>8. Max(P) = v BY <2>4, <2>5, <2>6, <2>7, <1>1, Order
  <2> QED BY <2>1, <2>4, <2>8
<1>4. /\ d + 2 \in Nat /\ d + 2 >= 5 /\ (d + 2) % 4 \in {0, 1} BY <1>1, <1>2
<1>5. \A m \in Reqs : m.step >= d + 2 => m.prop.key = v.key
  \* Propagate's parameters are constants: name the key and the step.
  <2>0. PICK k : k = v.key OBVIOUS
  <2>a. PICK t0 : t0 = d + 2 OBVIOUS
  <2>1. t0 \in Nat /\ t0 >= 5 /\ t0 % 4 \in {0, 1} BY <1>4, <2>a
  <2>2. \A m \in Reqs : m.step = t0 => m.prop.key = k BY <1>3, <2>0, <2>a
  <2>3. \A m \in Reqs : m.step >= t0 => m.prop.key = k BY <2>1, <2>2, Propagate
  <2> QED BY <2>0, <2>a, <2>3
<1> QED BY <1>5

\* q decided on the fast path: from step 5 on, every proposal has the leader's key.
LEMMA FastLock ==
    ASSUME IndInv, NEW q \in Proposers, pst[q].decided # Nil, pst[q].step = FirstStep
    PROVE  \A m \in Reqs : m.step >= 5 => m.prop.key = Leader
<1>1. /\ GotQ(q) \in Quorums
      /\ \A r \in Got(q) : r.first # Nil /\ r.first.prio = H
  BY DEF IndInv, DecInv, FirstStep
<1>2. \A m \in Reqs : m.step = 5 => m.prop.key = Leader
  <2> SUFFICES ASSUME NEW m \in Reqs, m.step = 5 PROVE m.prop.key = Leader OBVIOUS
  <2>1. PICK Q \in Quorums : /\ \A y \in Q : Entered(y, 4)
                             /\ \A y \in Q : Leq(FirstAt(y, 4), m.prop)
    BY DEF IndInv, SupportInv
  <2>2. PICK y \in Q \cap GotQ(q) : TRUE BY <1>1, QuorumsIntersect
  <2>3. PICK r \in pst[q].replies[y] : TRUE BY <2>2 DEF GotQ
  <2>4. r \in Got(q) /\ FirstAt(y, 4) = r.first
    BY <2>2, <2>3 DEF IndInv, GotInv, Got, GotQ, Quorums, FirstStep
  <2>5. m.prop.prio = H
    BY <1>1, <2>1, <2>2, <2>4, ReqTyped, ProposalFields DEF Leq, Gt
  <2> QED BY <2>5 DEF IndInv, HInv
<1>3. 5 \in Nat /\ 5 >= 5 /\ 5 % 4 \in {0, 1} OBVIOUS
<1> QED BY <1>2, <1>3, Propagate

\* A fast-path decision is the leader's value.
LEMMA FastDecision ==
    ASSUME IndInv, NEW q \in Proposers, pst[q].decided # Nil, pst[q].step = FirstStep
    PROVE  pst[q].decided.key = Leader
<1>1. PICK r \in Got(q) : r.first = pst[q].decided /\ r.first.prio = H
  BY DEF IndInv, DecInv, FirstStep
<1>2. PICK y \in Recorders : r \in pst[q].replies[y] BY <1>1 DEF Got
<1>3. /\ Entered(y, FirstStep) /\ FirstAt(y, FirstStep) = r.first
  BY <1>2 DEF IndInv, GotInv
<1>4. r.first \in ReqProps(FirstStep) BY <1>3, EntryFacts, RecProps
<1> QED BY <1>1, <1>4, ReqPropsTyped DEF IndInv, HInv

THEOREM IndAgreement == IndInv => Agreement
<1> SUFFICES ASSUME IndInv, NEW q1 \in Proposers, NEW q2 \in Proposers,
                    pst[q1].decided # Nil, pst[q2].decided # Nil
             PROVE  pst[q1].decided.key = pst[q2].decided.key
  BY DEF Agreement, Decided
<1> DEFINE Phase2(q) == pst[q].step % 4 = 2 /\ pst[q].decided = pst[q].p
           Fast(q) == pst[q].step = FirstStep
<1>1. \A q \in {q1, q2} : Phase2(q) \/ Fast(q) BY DEF IndInv, DecInv
<1>2. PICK x \in Recorders : TRUE BY Assumptions
\* A phase-2 decision is a request of its step (q sent its proposal to everyone).
<1>3. \A q \in {q1, q2} : Phase2(q) =>
          /\ \E m \in Reqs : m.step = pst[q].step /\ m.prop = pst[q].p
          /\ pst[q].step \in Nat /\ pst[q].step >= 6
  <2> SUFFICES ASSUME NEW q \in {q1, q2}, Phase2(q)
               PROVE  /\ \E m \in Reqs : m.step = pst[q].step /\ m.prop = pst[q].p
                      /\ pst[q].step \in Nat /\ pst[q].step >= 6
    OBVIOUS
  <2>1. pst[q].sent = [y \in Recorders |-> pst[q].p] /\ pst[q].step \in Nat
    BY DEF IndInv, SentInv, TypeInv, Drawing, FirstStep
  <2>2. [type |-> "req", from |-> q, to |-> x, step |-> pst[q].step, prop |-> pst[q].p] \in Reqs
    BY <1>2, <2>1 DEF IndInv, SentInv, Reqs
  <2>3. pst[q].step >= 6 BY <2>1 DEF IndInv, SentInv, FirstStep
  <2> QED BY <2>1, <2>2, <2>3
<1>4. CASE Phase2(q1) /\ Phase2(q2)
  <2>1. CASE pst[q1].step = pst[q2].step
    <3>1. Leq(pst[q2].p, pst[q1].p) BY <1>3, <1>4, <2>1, Phase2Bound
    <3>2. Leq(pst[q1].p, pst[q2].p) BY <1>3, <1>4, <2>1, Phase2Bound
    <3>3. pst[q1].p \in Proposal /\ pst[q2].p \in Proposal BY DEF IndInv, TypeInv
    <3> QED BY <1>4, <3>1, <3>2, <3>3, Order
  <2>2. CASE pst[q1].step < pst[q2].step
    <3>1. pst[q2].step >= pst[q1].step + 2 BY <1>3, <1>4, <2>2
    <3> QED BY <1>3, <1>4, <3>1, Phase2Lock
  <2>3. CASE pst[q2].step < pst[q1].step
    <3>1. pst[q1].step >= pst[q2].step + 2 BY <1>3, <1>4, <2>3
    <3> QED BY <1>3, <1>4, <3>1, Phase2Lock
  <2> QED BY <1>3, <1>4, <2>1, <2>2, <2>3
<1>5. CASE Phase2(q1) /\ Fast(q2)
  BY <1>3, <1>5, FastLock, FastDecision
<1>6. CASE Fast(q1) /\ Phase2(q2)
  BY <1>3, <1>6, FastLock, FastDecision
<1>7. CASE Fast(q1) /\ Fast(q2)
  BY <1>7, FastDecision
<1> QED BY <1>1, <1>4, <1>5, <1>6, <1>7
=============================================================================

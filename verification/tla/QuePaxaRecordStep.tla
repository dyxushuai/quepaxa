------------------------- MODULE QuePaxaRecordStep -------------------------
(***************************************************************************)
(* TLAPS proof that IndInv (QuePaxaProof.tla) holds initially and after   *)
(* a recorder records a request. QuePaxaInductive.tla covers the          *)
(* proposers' steps and states the theorems.                               *)
(***************************************************************************)
EXTENDS QuePaxaSafety

-----------------------------------------------------------------------------
(* More about the largest proposal *)

LEMMA MaxUnique ==
    ASSUME NEW S \in SUBSET Proposal, NEW c \in S, \A o \in S : Leq(o, c)
    PROVE  Max(S) = c
<1>1. Max(S) \in S /\ Leq(c, Max(S)) BY MaxProps
<1>2. Leq(Max(S), c) BY <1>1
<1> QED BY <1>1, <1>2, Order

LEMMA MaxAdd ==
    ASSUME NEW S \in SUBSET Proposal, S # {}, NEW p \in Proposal
    PROVE  Max(S \cup {p}) = IF Leq(p, Max(S)) THEN Max(S) ELSE p
<1>1. Max(S) \in S /\ \A o \in S : Leq(o, Max(S)) BY MaxProps
<1>2. CASE Leq(p, Max(S))
  <2>1. \A o \in S \cup {p} : Leq(o, Max(S)) BY <1>1, <1>2
  <2> QED BY <1>1, <1>2, <2>1, MaxUnique
<1>3. CASE ~Leq(p, Max(S))
  <2>1. Leq(Max(S), p) BY <1>1, <1>3, Order
  <2>2. \A o \in S \cup {p} : Leq(o, p) BY <1>1, <2>1, Order
  <2> QED BY <1>3, <2>2, MaxUnique
<1> QED BY <1>2, <1>3

LEMMA MaxSingle ==
    ASSUME NEW p \in Proposal
    PROVE  Max({p}) = p
BY Order, MaxUnique

-----------------------------------------------------------------------------
(* The initial state *)

THEOREM InitInv == HInit => IndInv
<1> SUFFICES ASSUME HInit PROVE IndInv OBVIOUS
<1>1. PICK sent \in [Proposers -> [Recorders -> [prio : Prios \cup {H}, key : Proposers]]] :
          /\ \A p \in Proposers : sent[p] \in Draws(p, FirstStep, [prio |-> H, key |-> p])
          /\ pst = [p \in Proposers |-> [step |-> FirstStep, p |-> [prio |-> H, key |-> p],
                      sent |-> sent[p], replies |-> [r \in Recorders |-> {}], decided |-> Nil]]
  BY DEF HInit, Init
<1>2. /\ reg = [r \in Recorders |-> [step |-> 0, first |-> Nil, agg |-> Nil, prev |-> Nil]]
      /\ msgs = UNION {RequestsOf(p, pst[p]) : p \in Proposers}
      /\ rec = {} /\ seen = {}
  BY DEF HInit, Init
\* What each proposer sent at step 4: the leader its H proposal, the others drawn priorities.
<1>3. \A p \in Proposers :
          /\ Drawing(p, FirstStep) =>
                 \A x \in Recorders : sent[p][x].key = p /\ sent[p][x].prio \in Prios
          /\ ~Drawing(p, FirstStep) =>
                 sent[p] = [x \in Recorders |-> [prio |-> H, key |-> p]] /\ p = Leader
  BY <1>1 DEF Draws, DrawPool, MayDraw, Drawing, FirstStep
<1>4. \A p \in Proposers : sent[p] \in [Recorders -> Proposal]
  BY <1>1, <1>3, Assumptions DEF Proposal, H
<1>a. \A m \in msgs : \E p \in Proposers, r \in Recorders :
          m = [type |-> "req", from |-> p, to |-> r, step |-> FirstStep, prop |-> sent[p][r]]
  BY <1>1, <1>2 DEF RequestsOf
<1>5. \A m \in msgs : /\ m.type = "req" /\ m.from \in Proposers /\ m.to \in Recorders
                      /\ m.step = FirstStep /\ m.prop = sent[m.from][m.to]
  BY <1>a
<1>6. Reqs = msgs /\ Reps = {} BY <1>5 DEF Reqs, Reps
<1>b. \A q \in Proposers :
          /\ pst[q].step = FirstStep /\ pst[q].p = [prio |-> H, key |-> q]
          /\ pst[q].sent = sent[q] /\ pst[q].replies = [r \in Recorders |-> {}]
          /\ pst[q].decided = Nil
  BY <1>1
<1>7. TypeInv
  <2>1. msgs \subseteq ReqMsg
    <3> SUFFICES ASSUME NEW m \in msgs PROVE m \in ReqMsg OBVIOUS
    <3>1. PICK p \in Proposers, r \in Recorders :
              m = [type |-> "req", from |-> p, to |-> r, step |-> FirstStep, prop |-> sent[p][r]]
      BY <1>a
    <3>2. sent[p][r] \in Proposal BY <1>4
    <3> QED BY <3>1, <3>2 DEF ReqMsg, FirstStep
  <2>2. \A q \in Proposers : [prio |-> H, key |-> q] \in Proposal BY DEF Proposal
  <2>3. pst \in [Proposers -> [step : Nat, p : Proposal, sent : [Recorders -> Proposal],
                               replies : [Recorders -> SUBSET RepMsg],
                               decided : Proposal \cup {Nil}]]
    BY <1>1, <1>4, <2>2 DEF FirstStep
  <2>4. reg \in [Recorders -> [step : Nat, first : Proposal \cup {Nil},
                               agg : Proposal \cup {Nil}, prev : Proposal \cup {Nil}]]
    BY <1>2
  <2> QED BY <1>2, <2>1, <2>3, <2>4 DEF TypeInv
<1>8. SentInv
  <2> SUFFICES ASSUME NEW q \in Proposers
               PROVE  LET s == pst[q] IN
                      /\ s.step >= FirstStep
                      /\ Drawing(q, s.step) =>
                             \A x \in Recorders : s.sent[x].key = s.p.key /\ s.sent[x].prio \in Prios
                      /\ ~Drawing(q, s.step) => s.sent = [x \in Recorders |-> s.p]
                      /\ \A x \in Recorders :
                             [type |-> "req", from |-> q, to |-> x, step |-> s.step,
                              prop |-> s.sent[x]] \in msgs
    BY DEF SentInv
  <2>0. RequestsOf(q, pst[q]) \subseteq msgs BY <1>2
  <2>1. \A x \in Recorders :
            [type |-> "req", from |-> q, to |-> x, step |-> pst[q].step,
             prop |-> pst[q].sent[x]] \in msgs
    BY <2>0 DEF RequestsOf
  <2>2. pst[q].step >= FirstStep BY <1>b DEF FirstStep
  <2>3. Drawing(q, pst[q].step) =>
            \A x \in Recorders : pst[q].sent[x].key = pst[q].p.key /\ pst[q].sent[x].prio \in Prios
    BY <1>3, <1>b
  <2>4. ~Drawing(q, pst[q].step) => pst[q].sent = [x \in Recorders |-> pst[q].p]
    BY <1>3, <1>b
  <2> QED BY <2>1, <2>2, <2>3, <2>4
<1>9. HInv
  <2> SUFFICES ASSUME NEW m \in Reqs, m.prop.prio = H PROVE m.prop.key = Leader BY DEF HInv
  <2>1. ~Drawing(m.from, FirstStep) BY <1>3, <1>5, <1>6, Assumptions DEF H
  <2> QED BY <1>3, <1>5, <1>6, <2>1
<1>10. SupportInv BY <1>5, <1>6 DEF SupportInv, FirstStep
<1>11. RegInv BY <1>2 DEF RegInv
<1>12. SeenInv BY <1>2 DEF SeenInv
<1>13. ReqInv
  <2> SUFFICES ASSUME NEW m \in Reqs
               PROVE  /\ m.step >= FirstStep
                      /\ m.step <= pst[m.from].step
                      /\ m.step = pst[m.from].step => m.prop = pst[m.from].sent[m.to]
    BY DEF ReqInv
  <2>1. m \in msgs BY DEF Reqs
  <2>2. m.from \in Proposers /\ m.step = FirstStep /\ m.prop = sent[m.from][m.to] BY <1>5, <2>1
  <2> QED BY <2>2, <1>b DEF FirstStep
<1>14. RepInv /\ GotInv /\ DecInv BY <1>6, <1>b DEF RepInv, GotInv, DecInv
<1> QED BY <1>7, <1>8, <1>9, <1>10, <1>11, <1>12, <1>13, <1>14 DEF IndInv
-----------------------------------------------------------------------------
(* A recorder records a request *)

RegT == [step : Nat, first : Proposal \cup {Nil}, agg : Proposal \cup {Nil},
         prev : Proposal \cup {Nil}]

LEMMA RecordStep ==
    ASSUME IndInv, NEW m \in msgs, RecordH(m)
    PROVE  IndInv'
\* g is the recorder's register before the step: a constant, so priming leaves it alone.
<1>0. PICK g : g = reg[m.to] OBVIOUS
<1> DEFINE x == m.to
           pv == IF g.step + 1 = m.step THEN g.agg ELSE Nil
           new == IF m.step = g.step
                  THEN IF g.agg # Nil /\ ~Gt(m.prop, g.agg) THEN g ELSE [g EXCEPT !.agg = m.prop]
                  ELSE IF m.step > g.step
                       THEN [step |-> m.step, first |-> m.prop, agg |-> m.prop, prev |-> pv]
                       ELSE g
           rp == [type |-> "rep", from |-> m.to, to |-> m.from, req |-> m.step,
                  step |-> new.step, first |-> new.first, prev |-> new.prev]
           e == [r |-> m.to, step |-> m.step, prop |-> m.prop]
           sn == [r |-> m.to, step |-> m.step, first |-> m.prop, prev |-> pv]
<1>1. /\ m \in Reqs
      /\ reg' = [reg EXCEPT ![x] = new]
      /\ msgs' = msgs \cup {rp}
      /\ pst' = pst
      /\ rec' = IF m.step >= g.step THEN rec \cup {e} ELSE rec
      /\ seen' = IF m.step > g.step THEN seen \cup {sn} ELSE seen
  BY <1>0 DEF RecordH, Record, Reqs
<1>2. /\ m.prop \in Proposal /\ m.step \in Nat /\ m.from \in Proposers /\ x \in Recorders
      /\ m.step >= FirstStep /\ m.step <= pst[m.from].step
      /\ m.step = pst[m.from].step => m.prop = pst[m.from].sent[x]
  BY <1>1, ReqTyped DEF IndInv, ReqInv
<1>3. /\ g \in RegT
      /\ g.step # 0 => /\ Entered(x, g.step) /\ FirstAt(x, g.step) = g.first
                       /\ PrevAt(x, g.step) = g.prev
                       /\ g.first \in RecAt(x, g.step) /\ g.agg = Max(RecAt(x, g.step))
      /\ \A e2 \in rec : e2.r = x => e2.step <= g.step
      /\ \A s2 \in seen : s2.r = x => s2.step <= g.step
  BY <1>0, <1>2 DEF IndInv, TypeInv, RegInv, RegT
<1>4. Reqs' = Reqs /\ Reps' = Reps \cup {rp} BY <1>1 DEF Reqs, Reps
<1>5. \A t : ReqProps(t)' = ReqProps(t) BY <1>4 DEF ReqProps
\* Recorded sets only grow, and only at (x, m.step).
<1>6. \A y, t : /\ RecAt(y, t) \subseteq RecAt(y, t)'
                /\ (y # x \/ t # m.step) => RecAt(y, t)' = RecAt(y, t)
                /\ m.step >= g.step => RecAt(x, m.step)' = RecAt(x, m.step) \cup {m.prop}
                /\ m.step < g.step => RecAt(y, t)' = RecAt(y, t)
  <2>1. rec \subseteq rec' /\ rec' \subseteq rec \cup {e} BY <1>1
  <2>2. \A y, t : RecAt(y, t) \subseteq RecAt(y, t)' BY <2>1 DEF RecAt
  <2>3. \A y, t : (y # x \/ t # m.step) => RecAt(y, t)' = RecAt(y, t)
    <3> SUFFICES ASSUME NEW y, NEW t, y # x \/ t # m.step PROVE RecAt(y, t)' \subseteq RecAt(y, t)
      BY <2>2
    <3> SUFFICES ASSUME NEW w \in RecAt(y, t)' PROVE w \in RecAt(y, t) OBVIOUS
    <3>1. PICK e2 \in rec' : e2.r = y /\ e2.step = t /\ e2.prop = w BY DEF RecAt
    <3>2. e2 # e
      <4>1. e.r = x /\ e.step = m.step OBVIOUS
      <4> QED BY <3>1, <4>1
    <3> QED BY <2>1, <3>1, <3>2 DEF RecAt
  <2>4. m.step >= g.step => RecAt(x, m.step)' = RecAt(x, m.step) \cup {m.prop}
    <3> SUFFICES ASSUME m.step >= g.step PROVE RecAt(x, m.step)' = RecAt(x, m.step) \cup {m.prop}
      OBVIOUS
    <3>1. rec' = rec \cup {e} BY <1>1
    <3> QED BY <3>1 DEF RecAt
  <2>5. m.step < g.step => \A y, t : RecAt(y, t)' = RecAt(y, t)
    <3>1. m.step < g.step => rec' = rec BY <1>1, <1>2, <1>3 DEF RegT
    <3> QED BY <3>1 DEF RecAt
  <2> QED BY <2>2, <2>3, <2>4, <2>5
\* A recorder never entered a step above its own.
<1>7. \A t : Entered(x, t) => t <= g.step BY <1>3 DEF Entered, SeenAt
<1>8. \A t : t > g.step => RecAt(x, t) = {}
  <2> SUFFICES ASSUME NEW t, t > g.step, NEW w \in RecAt(x, t) PROVE FALSE OBVIOUS
  <2>1. PICK e2 \in rec : e2.r = x /\ e2.step = t /\ e2.prop = w BY DEF RecAt
  <2>2. e2.step <= g.step BY <2>1, <1>3
  <2>3. e2.step \in Nat /\ g.step \in Nat BY <2>1, <1>3 DEF IndInv, TypeInv, RegT
  <2> QED BY <2>1, <2>2, <2>3
\* Steps already entered keep their entry.
<1>9. \A y, t : Entered(y, t) =>
          /\ SeenAt(y, t)' = SeenAt(y, t) /\ Entered(y, t)'
          /\ EntryOf(y, t)' = EntryOf(y, t)
          /\ FirstAt(y, t)' = FirstAt(y, t) /\ PrevAt(y, t)' = PrevAt(y, t)
  <2> SUFFICES ASSUME NEW y, NEW t, Entered(y, t) PROVE SeenAt(y, t)' = SeenAt(y, t)
    BY DEF Entered, EntryOf, FirstAt, PrevAt
  <2>1. CASE m.step > g.step
    <3>1. ~(y = x /\ t = m.step) BY <1>2, <1>3, <1>7, <2>1 DEF RegT
    <3> QED BY <1>1, <2>1, <3>1 DEF SeenAt
  <2>2. CASE ~(m.step > g.step) BY <1>1, <2>2 DEF SeenAt
  <2> QED BY <2>1, <2>2
<1>10. \A y, t : Entered(y, t) => Entered(y, t)' BY <1>9
\* The new register, against the new history.
<1>11. /\ new \in RegT
       /\ new.step \in Nat /\ new.step >= m.step /\ new.step >= g.step /\ new.step # 0
       /\ Entered(x, new.step)'
       /\ FirstAt(x, new.step)' = new.first
       /\ PrevAt(x, new.step)' = new.prev
       /\ new.first \in RecAt(x, new.step)'
       /\ new.agg = Max(RecAt(x, new.step)')
       /\ new.step = m.step => m.prop \in RecAt(x, m.step)'
       /\ m.step > g.step => /\ new.step = m.step /\ sn \in seen'
                             /\ SeenAt(x, m.step)' = {sn}
                             /\ pv = MaxOf(RecAt(x, m.step - 1)')
  <2>1. CASE m.step > g.step
    <3>1. new = [step |-> m.step, first |-> m.prop, agg |-> m.prop, prev |-> pv]
      BY <2>1, <1>2, <1>3 DEF RegT
    <3>2. RecAt(x, m.step) = {} BY <1>8, <2>1
    <3>3. RecAt(x, m.step)' = {m.prop} BY <1>6, <2>1, <3>2, <1>2, <1>3 DEF RegT
    <3>4. SeenAt(x, m.step)' = {sn}
      <4>1. SeenAt(x, m.step) = {} BY <1>7, <2>1, <1>2, <1>3 DEF Entered, RegT
      <4> QED BY <1>1, <2>1, <4>1 DEF SeenAt
    <3>5. pv \in Proposal \cup {Nil} BY <1>3 DEF RegT
    \* pv is the best recorded at m.step - 1, if x was there.
    <3>6. pv = MaxOf(RecAt(x, m.step - 1)')
      <4>1. RecAt(x, m.step - 1)' = RecAt(x, m.step - 1) BY <1>6, <1>2
      <4>2. CASE g.step + 1 = m.step
        <5>1. g.step # 0 BY <4>2, <1>2, <1>3 DEF RegT, FirstStep
        <5>2. RecAt(x, g.step) # {} /\ g.agg = Max(RecAt(x, g.step)) BY <5>1, <1>3
        <5>3. m.step - 1 = g.step BY <4>2, <1>2, <1>3 DEF RegT
        <5> QED BY <4>1, <4>2, <5>2, <5>3 DEF MaxOf
      <4>3. CASE g.step + 1 # m.step
        <5>1. m.step - 1 > g.step BY <4>3, <2>1, <1>2, <1>3 DEF RegT
        <5> QED BY <4>1, <4>3, <5>1, <1>8 DEF MaxOf
      <4> QED BY <4>2, <4>3
    <3>7. EntryOf(x, m.step)' = sn BY <3>4 DEF EntryOf
    <3> QED BY <3>1, <3>3, <3>4, <3>5, <3>6, <3>7, <1>1, <1>2, <2>1, MaxSingle
      DEF RegT, Entered, FirstAt, PrevAt, FirstStep
  <2>2. CASE m.step = g.step
    <3>1. g.step # 0 BY <2>2, <1>2 DEF FirstStep
    <3>2. /\ Entered(x, g.step) /\ FirstAt(x, g.step) = g.first /\ PrevAt(x, g.step) = g.prev
          /\ g.first \in RecAt(x, g.step) /\ g.agg = Max(RecAt(x, g.step))
      BY <3>1, <1>3
    <3>3. RecAt(x, g.step) \subseteq Proposal /\ RecAt(x, g.step) # {} BY <3>2, RecProps
    <3>4. RecAt(x, g.step)' = RecAt(x, g.step) \cup {m.prop} BY <1>6, <2>2, <1>2, <1>3 DEF RegT
    <3>5. g.agg # Nil /\ g.agg \in Proposal BY <3>2, <3>3, MaxProps, ProposalFields
    <3>6. new = IF Leq(m.prop, g.agg) THEN g ELSE [g EXCEPT !.agg = m.prop]
      BY <2>2, <3>5 DEF Leq
    <3>7. new.agg = Max(RecAt(x, g.step)')
      <4>1. Max(RecAt(x, g.step) \cup {m.prop}) = IF Leq(m.prop, g.agg) THEN g.agg ELSE m.prop
        BY <3>2, <3>3, <1>2, MaxAdd
      <4>2. new.agg = IF Leq(m.prop, g.agg) THEN g.agg ELSE m.prop BY <3>6, <1>3 DEF RegT
      <4> QED BY <3>4, <4>1, <4>2
    <3>8. new.step = g.step /\ new.first = g.first /\ new.prev = g.prev /\ new \in RegT
      BY <3>6, <1>3, <1>2 DEF RegT
    <3> QED BY <2>2, <3>1, <3>2, <3>4, <3>7, <3>8, <1>9, <1>3 DEF RegT
  <2>3. CASE m.step < g.step
    <3>1. new = g BY <2>3, <1>2, <1>3 DEF RegT
    <3>2. g.step # 0 BY <2>3, <1>2, <1>3 DEF RegT, FirstStep
    <3>3. /\ Entered(x, g.step) /\ FirstAt(x, g.step) = g.first /\ PrevAt(x, g.step) = g.prev
          /\ g.first \in RecAt(x, g.step) /\ g.agg = Max(RecAt(x, g.step))
      BY <3>2, <1>3
    <3>4. RecAt(x, g.step)' = RecAt(x, g.step) BY <1>6, <2>3
    <3> QED BY <2>3, <3>1, <3>2, <3>3, <3>4, <1>9, <1>3 DEF RegT
  <2> QED BY <2>1, <2>2, <2>3, <1>2, <1>3 DEF RegT
<1>12. TypeInv'
  <2>1. reg' \in [Recorders -> RegT] BY <1>1, <1>11 DEF IndInv, TypeInv, RegT
  <2>2. rp \in RepMsg BY <1>2, <1>11 DEF RepMsg, RegT, FirstStep
  <2>3. e \in [r : Recorders, step : Nat, prop : Proposal] BY <1>2
  <2>4. sn \in [r : Recorders, step : Nat, first : Proposal, prev : Proposal \cup {Nil}]
    BY <1>2, <1>3 DEF RegT
  <2> QED BY <1>1, <2>1, <2>2, <2>3, <2>4 DEF IndInv, TypeInv, RegT
<1>13. RegInv'
  <2> SUFFICES ASSUME NEW y \in Recorders
               PROVE  /\ \A e2 \in rec' : e2.r = y => e2.step <= reg'[y].step
                      /\ \A s2 \in seen' : s2.r = y => s2.step <= reg'[y].step
                      /\ reg'[y].step = 0 =>
                             /\ reg'[y].first = Nil /\ reg'[y].agg = Nil /\ reg'[y].prev = Nil
                      /\ reg'[y].step # 0 =>
                             /\ Entered(y, reg[y].step)'
                             /\ FirstAt(y, reg[y].step)' = reg'[y].first
                             /\ PrevAt(y, reg[y].step)' = reg'[y].prev
                             /\ reg'[y].first \in RecAt(y, reg[y].step)'
                             /\ reg'[y].agg = Max(RecAt(y, reg[y].step)')
    BY DEF RegInv
  <2>1. CASE y = x
    <3>1. reg'[y] = new BY <1>1, <2>1, <1>2 DEF IndInv, TypeInv
    <3>2. \A e2 \in rec' : e2.r = y => e2.step <= reg'[y].step
      <4> SUFFICES ASSUME NEW e2 \in rec', e2.r = y PROVE e2.step <= new.step BY <3>1
      <4>1. CASE e2 \in rec BY <4>1, <2>1, <1>3, <1>11, <1>2 DEF IndInv, TypeInv, RegT
      <4>2. CASE e2 = e BY <4>2, <1>11
      <4> QED BY <1>1, <4>1, <4>2
    <3>3. \A s2 \in seen' : s2.r = y => s2.step <= reg'[y].step
      <4> SUFFICES ASSUME NEW s2 \in seen', s2.r = y PROVE s2.step <= new.step BY <3>1
      <4>1. CASE s2 \in seen BY <4>1, <2>1, <1>3, <1>11, <1>2 DEF IndInv, TypeInv, RegT
      <4>2. CASE s2 = sn BY <4>2, <1>11
      <4> QED BY <1>1, <4>1, <4>2
    <3>4. reg'[y].step # 0 /\ reg'[y].step = new.step BY <3>1, <1>11
    <3> QED BY <3>1, <3>2, <3>3, <3>4, <2>1, <1>11
  <2>2. CASE y # x
    <3>1. reg'[y] = reg[y] BY <1>1, <2>2 DEF IndInv, TypeInv
    <3>2. /\ \A e2 \in rec : e2.r = y => e2.step <= reg[y].step
          /\ \A s2 \in seen : s2.r = y => s2.step <= reg[y].step
          /\ reg[y].step = 0 => reg[y].first = Nil /\ reg[y].agg = Nil /\ reg[y].prev = Nil
          /\ reg[y].step # 0 =>
                 /\ Entered(y, reg[y].step) /\ FirstAt(y, reg[y].step) = reg[y].first
                 /\ PrevAt(y, reg[y].step) = reg[y].prev
                 /\ reg[y].first \in RecAt(y, reg[y].step)
                 /\ reg[y].agg = Max(RecAt(y, reg[y].step))
      BY DEF IndInv, RegInv
    <3>3. \A t : RecAt(y, t)' = RecAt(y, t) BY <1>6, <2>2
    <3> QED BY <1>1, <2>2, <3>1, <3>2, <3>3, <1>9
  <2> QED BY <2>1, <2>2
<1>14. SeenInv'
  <2>1. \A s2 \in seen' :
            /\ s2.step >= FirstStep
            /\ \A s3 \in seen' : s3.r = s2.r /\ s3.step = s2.step => s3 = s2
            /\ s2.first \in RecAt(s2.r, s2.step)'
            /\ s2.prev = MaxOf(RecAt(s2.r, s2.step - 1)')
    <3> SUFFICES ASSUME NEW s2 \in seen'
                 PROVE  /\ s2.step >= FirstStep
                        /\ \A s3 \in seen' : s3.r = s2.r /\ s3.step = s2.step => s3 = s2
                        /\ s2.first \in RecAt(s2.r, s2.step)'
                        /\ s2.prev = MaxOf(RecAt(s2.r, s2.step - 1)')
      OBVIOUS
    <3>1. CASE s2 = sn /\ m.step > g.step
      <4>1. \A s3 \in seen' : s3.r = x /\ s3.step = m.step => s3 = sn BY <1>11, <3>1 DEF SeenAt
      <4> QED BY <3>1, <4>1, <1>11, <1>2
    <3>2. CASE s2 \in seen
      <4>1. /\ s2.step >= FirstStep
            /\ \A s3 \in seen : s3.r = s2.r /\ s3.step = s2.step => s3 = s2
            /\ s2.first \in RecAt(s2.r, s2.step)
            /\ s2.prev = MaxOf(RecAt(s2.r, s2.step - 1))
            /\ s2.r \in Recorders /\ s2.step \in Nat
        BY <3>2 DEF IndInv, SeenInv, TypeInv
      <4>2. s2.r = x => s2.step <= g.step BY <3>2, <1>3
      <4>3. \A s3 \in seen' : s3.r = s2.r /\ s3.step = s2.step => s3 = s2
        BY <1>1, <4>1, <4>2, <1>2, <1>3 DEF RegT
      <4>4. RecAt(s2.r, s2.step - 1)' = RecAt(s2.r, s2.step - 1)
        BY <1>6, <4>2, <4>1, <1>2, <1>3 DEF RegT
      <4> QED BY <4>1, <4>3, <4>4, <1>6
    <3> QED BY <1>1, <3>1, <3>2
  <2>2. \A e2 \in rec' : Entered(e2.r, e2.step)'
    <3> SUFFICES ASSUME NEW e2 \in rec' PROVE Entered(e2.r, e2.step)' OBVIOUS
    <3>1. CASE e2 \in rec BY <3>1, <1>10 DEF IndInv, SeenInv
    <3>2. CASE e2 = e /\ m.step >= g.step
      <4>1. new.step = m.step BY <3>2, <1>2, <1>3 DEF RegT
      <4> QED BY <3>2, <4>1, <1>11
    <3> QED BY <1>1, <3>1, <3>2
  <2>3. \A e2 \in rec' : \E m2 \in Reqs' : m2.to = e2.r /\ m2.step = e2.step /\ m2.prop = e2.prop
    BY <1>1, <1>4 DEF IndInv, SeenInv
  <2> QED BY <2>1, <2>2, <2>3 DEF SeenInv
<1>15. ReqInv' BY <1>1, <1>4 DEF IndInv, ReqInv
<1>16. SentInv' BY <1>1 DEF IndInv, SentInv, Drawing
<1>17. RepInv'
  <2> SUFFICES ASSUME NEW r \in Reps'
               PROVE  /\ Entered(r.from, r.step)'
                      /\ FirstAt(r.from, r.step)' = r.first
                      /\ PrevAt(r.from, r.step)' = r.prev
                      /\ r.req >= FirstStep
                      /\ r.step >= r.req
                      /\ r.req <= pst'[r.to].step
                      /\ r.req = pst'[r.to].step /\ r.step = r.req =>
                             pst'[r.to].sent[r.from] \in RecAt(r.from, r.step)'
    BY DEF RepInv
  <2>1. CASE r \in Reps
    <3>1. /\ Entered(r.from, r.step) /\ FirstAt(r.from, r.step) = r.first
          /\ PrevAt(r.from, r.step) = r.prev /\ r.req >= FirstStep /\ r.step >= r.req
          /\ r.req <= pst[r.to].step
          /\ r.req = pst[r.to].step /\ r.step = r.req =>
                 pst[r.to].sent[r.from] \in RecAt(r.from, r.step)
      BY <2>1 DEF IndInv, RepInv
    <3> QED BY <3>1, <1>1, <1>6, <1>9
  <2>2. CASE r = rp
    <3>1. r.step >= r.req BY <2>2, <1>11
    <3>2. r.req = pst'[r.to].step /\ r.step = r.req =>
              pst'[r.to].sent[r.from] \in RecAt(r.from, r.step)'
      BY <2>2, <1>1, <1>2, <1>11
    <3> QED BY <2>2, <3>1, <3>2, <1>1, <1>2, <1>11
  <2> QED BY <1>4, <2>1, <2>2
<1>18. GotInv'
  <2> SUFFICES ASSUME NEW q \in Proposers, NEW y \in Recorders, NEW r \in pst'[q].replies[y]
               PROVE  /\ r.from = y
                      /\ r.req = pst'[q].step /\ r.step = pst'[q].step
                      /\ Entered(y, r.step)'
                      /\ FirstAt(y, r.step)' = r.first
                      /\ PrevAt(y, r.step)' = r.prev
                      /\ pst'[q].sent[y] \in RecAt(y, r.step)'
    BY DEF GotInv
  <2>1. /\ r.from = y /\ r.req = pst[q].step /\ r.step = pst[q].step
        /\ Entered(y, r.step) /\ FirstAt(y, r.step) = r.first /\ PrevAt(y, r.step) = r.prev
        /\ pst[q].sent[y] \in RecAt(y, r.step)
    BY <1>1 DEF IndInv, GotInv
  <2> QED BY <2>1, <1>1, <1>6, <1>9
<1>19. DecInv' BY <1>1 DEF IndInv, DecInv, GotQ, Got
<1>20. HInv' BY <1>4 DEF IndInv, HInv
<1>21. SupportInv'
  <2> SUFFICES ASSUME NEW m2 \in Reqs'
               PROVE  /\ m2.step % 4 = 1 =>
                             \E Q \in Quorums :
                                 /\ \A y \in Q : Entered(y, m2.step - 1)'
                                 /\ \E y \in Q : FirstAt(y, m2.step - 1)' = m2.prop
                                 /\ \A y \in Q : Leq(FirstAt(y, m2.step - 1)', m2.prop)
                      /\ m2.step % 4 = 2 =>
                             /\ m2.prop \in ReqProps(m2.step - 1)'
                             /\ \E Q \in Quorums : \A y \in Q :
                                    \E u \in RecAt(y, m2.step - 1)' : Leq(m2.prop, u)
                      /\ m2.step % 4 = 3 => m2.prop \in ReqProps(m2.step - 1)'
                      /\ m2.step % 4 = 0 /\ m2.step > FirstStep =>
                             \E Q \in Quorums :
                                 /\ \A y \in Q : Entered(y, m2.step - 1)'
                                 /\ LET P == {PrevAt(y, m2.step - 1)' : y \in Q} \ {Nil}
                                    IN IF P # {} THEN m2.prop.key = Max(P).key
                                       ELSE \E w \in ReqProps(m2.step - 1)' : w.key = m2.prop.key
    BY DEF SupportInv
  <2>0. m2 \in Reqs BY <1>4
  <2>1. m2.step % 4 = 1 =>
            \E Q \in Quorums :
                /\ \A y \in Q : Entered(y, m2.step - 1)'
                /\ \E y \in Q : FirstAt(y, m2.step - 1)' = m2.prop
                /\ \A y \in Q : Leq(FirstAt(y, m2.step - 1)', m2.prop)
    <3> SUFFICES ASSUME m2.step % 4 = 1
                 PROVE  \E Q \in Quorums :
                            /\ \A y \in Q : Entered(y, m2.step - 1)'
                            /\ \E y \in Q : FirstAt(y, m2.step - 1)' = m2.prop
                            /\ \A y \in Q : Leq(FirstAt(y, m2.step - 1)', m2.prop)
      OBVIOUS
    <3>1. PICK Q \in Quorums : /\ \A y \in Q : Entered(y, m2.step - 1)
                               /\ \E y \in Q : FirstAt(y, m2.step - 1) = m2.prop
                               /\ \A y \in Q : Leq(FirstAt(y, m2.step - 1), m2.prop)
      BY <2>0 DEF IndInv, SupportInv
    <3> QED BY <3>1, <1>9
  <2>2. m2.step % 4 = 2 =>
            /\ m2.prop \in ReqProps(m2.step - 1)'
            /\ \E Q \in Quorums : \A y \in Q : \E u \in RecAt(y, m2.step - 1)' : Leq(m2.prop, u)
    <3> SUFFICES ASSUME m2.step % 4 = 2
                 PROVE  /\ m2.prop \in ReqProps(m2.step - 1)'
                        /\ \E Q \in Quorums : \A y \in Q :
                               \E u \in RecAt(y, m2.step - 1)' : Leq(m2.prop, u)
      OBVIOUS
    <3>1. /\ m2.prop \in ReqProps(m2.step - 1)
          /\ \E Q \in Quorums : \A y \in Q : \E u \in RecAt(y, m2.step - 1) : Leq(m2.prop, u)
      BY <2>0 DEF IndInv, SupportInv
    <3> QED BY <3>1, <1>5, <1>6
  <2>3. m2.step % 4 = 3 => m2.prop \in ReqProps(m2.step - 1)'
    BY <2>0, <1>5 DEF IndInv, SupportInv
  <2>4. m2.step % 4 = 0 /\ m2.step > FirstStep =>
            \E Q \in Quorums :
                /\ \A y \in Q : Entered(y, m2.step - 1)'
                /\ LET P == {PrevAt(y, m2.step - 1)' : y \in Q} \ {Nil}
                   IN IF P # {} THEN m2.prop.key = Max(P).key
                      ELSE \E w \in ReqProps(m2.step - 1)' : w.key = m2.prop.key
    <3> SUFFICES ASSUME m2.step % 4 = 0, m2.step > FirstStep
                 PROVE  \E Q \in Quorums :
                            /\ \A y \in Q : Entered(y, m2.step - 1)'
                            /\ LET P == {PrevAt(y, m2.step - 1)' : y \in Q} \ {Nil}
                               IN IF P # {} THEN m2.prop.key = Max(P).key
                                  ELSE \E w \in ReqProps(m2.step - 1)' : w.key = m2.prop.key
      OBVIOUS
    <3>1. PICK Q \in Quorums :
              /\ \A y \in Q : Entered(y, m2.step - 1)
              /\ LET P == {PrevAt(y, m2.step - 1) : y \in Q} \ {Nil}
                 IN IF P # {} THEN m2.prop.key = Max(P).key
                    ELSE \E w \in ReqProps(m2.step - 1) : w.key = m2.prop.key
      BY <2>0 DEF IndInv, SupportInv
    <3>2. {PrevAt(y, m2.step - 1)' : y \in Q} = {PrevAt(y, m2.step - 1) : y \in Q}
      BY <3>1, <1>9 DEF Quorums
    <3> QED BY <3>1, <3>2, <1>5, <1>9
  <2> QED BY <2>1, <2>2, <2>3, <2>4
<1> QED BY <1>12, <1>13, <1>14, <1>15, <1>16, <1>17, <1>18, <1>19, <1>20, <1>21 DEF IndInv
=============================================================================

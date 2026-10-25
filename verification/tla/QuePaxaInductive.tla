------------------------- MODULE QuePaxaInductive -------------------------
(***************************************************************************)
(* TLAPS proof that IndInv (QuePaxaProof.tla) is inductive, and so, with  *)
(* QuePaxaSafety.tla, that every behaviour of HSpec satisfies Agreement.  *)
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
-----------------------------------------------------------------------------
(* A proposer changes its own state *)

PstT == [step : Nat, p : Proposal, sent : [Recorders -> Proposal],
         replies : [Recorders -> SUBSET RepMsg], decided : Proposal \cup {Nil}]

\* GotInv, DecInv, SentInv and SupportInv for one proposer state or request.
GotOK(s) ==
    \A y \in Recorders : \A r \in s.replies[y] :
        /\ r.from = y /\ r.req = s.step /\ r.step = s.step
        /\ Entered(y, r.step) /\ FirstAt(y, r.step) = r.first /\ PrevAt(y, r.step) = r.prev
        /\ s.sent[y] \in RecAt(y, r.step)
QS(s) == {y \in Recorders : s.replies[y] # {}}
RS(s) == UNION {s.replies[y] : y \in Recorders}
DecOK(s) ==
    s.decided # Nil =>
        /\ QS(s) \in Quorums
        /\ \/ /\ s.step % 4 = 2 /\ s.decided = s.p
              /\ \A r \in RS(s) : r.prev # Nil => Leq(r.prev, s.p)
              /\ \E r \in RS(s) : r.prev = s.p
           \/ /\ s.step = FirstStep
              /\ \A r \in RS(s) : r.first # Nil /\ r.first.prio = H
              /\ \E r \in RS(s) : r.first = s.decided
SentOK(q, s) ==
    /\ s.step >= FirstStep
    /\ Drawing(q, s.step) => \A x \in Recorders : s.sent[x].key = s.p.key /\ s.sent[x].prio \in Prios
    /\ ~Drawing(q, s.step) => s.sent = [x \in Recorders |-> s.p]
Supported(t, w) ==
    /\ t % 4 = 1 =>
           \E Q \in Quorums :
               /\ \A y \in Q : Entered(y, t - 1)
               /\ \E y \in Q : FirstAt(y, t - 1) = w
               /\ \A y \in Q : Leq(FirstAt(y, t - 1), w)
    /\ t % 4 = 2 =>
           /\ w \in ReqProps(t - 1)
           /\ \E Q \in Quorums : \A y \in Q : \E u \in RecAt(y, t - 1) : Leq(w, u)
    /\ t % 4 = 3 => w \in ReqProps(t - 1)
    /\ t % 4 = 0 /\ t > FirstStep =>
           \E Q \in Quorums :
               /\ \A y \in Q : Entered(y, t - 1)
               /\ LET P == {PrevAt(y, t - 1) : y \in Q} \ {Nil}
                  IN IF P # {} THEN w.key = Max(P).key
                     ELSE \E w2 \in ReqProps(t - 1) : w2.key = w.key

LEMMA RepTyped ==
    ASSUME IndInv, NEW r \in Reps
    PROVE  r \in RepMsg
<1>1. r \in msgs /\ r.type = "rep" BY DEF Reps
<1>2. r \in ReqMsg \cup RepMsg BY <1>1 DEF IndInv, TypeInv
<1>3. r \notin ReqMsg BY <1>1 DEF ReqMsg
<1> QED BY <1>2, <1>3

LEMMA InvForms ==
    ASSUME IndInv
    PROVE  /\ \A q \in Proposers : GotOK(pst[q]) /\ DecOK(pst[q]) /\ SentOK(q, pst[q])
           /\ \A m \in Reqs : Supported(m.step, m.prop)
BY DEF IndInv, GotInv, DecInv, SentInv, SupportInv, GotOK, DecOK, SentOK, Supported, QS, RS,
       GotQ, Got

\* At a phase-0 step only the key of a request's proposal matters for its support.
LEMMA SupportedKey ==
    ASSUME NEW t, NEW w, NEW w2, Supported(t, w), t % 4 = 0, w2.key = w.key
    PROVE  Supported(t, w2)
BY DEF Supported

\* Support survives any step that keeps the history and only adds requests.
LEMMA SupportedMono ==
    ASSUME UNCHANGED <<rec, seen>>, Reqs \subseteq Reqs', NEW t, NEW w, Supported(t, w)
    PROVE  Supported(t, w)'
<1>1. \A y, u : /\ RecAt(y, u)' = RecAt(y, u) /\ Entered(y, u)' = Entered(y, u)
                /\ FirstAt(y, u)' = FirstAt(y, u) /\ PrevAt(y, u)' = PrevAt(y, u)
  BY DEF RecAt, Entered, SeenAt, FirstAt, PrevAt, EntryOf
<1>2. \A u : ReqProps(u) \subseteq ReqProps(u)' BY DEF ReqProps
<1>3. \A Q \in Quorums : {PrevAt(y, t - 1)' : y \in Q} = {PrevAt(y, t - 1) : y \in Q} BY <1>1
<1> QED BY <1>1, <1>2, <1>3 DEF Supported

\* IndInv survives a step in which proposer q moves to `ns`, the reply m is consumed, and
\* q's requests for a new step are sent, provided `ns` is locally consistent.
LEMMA ProposerStep ==
    ASSUME IndInv, NEW m \in Reps, NEW q \in Proposers, NEW ns,
           UNCHANGED <<reg, rec, seen>>,
           pst' = [pst EXCEPT ![q] = ns],
           msgs' = (msgs \ {m}) \cup (IF ns.step # pst[q].step THEN RequestsOf(q, ns) ELSE {}),
           ns \in PstT,
           ns.step >= pst[q].step,
           ns.step = pst[q].step => ns.sent = pst[q].sent /\ ns.p = pst[q].p,
           GotOK(ns), DecOK(ns), SentOK(q, ns),
           ns.step # pst[q].step =>
               \A x \in Recorders : /\ Supported(ns.step, ns.sent[x])
                                    /\ ns.sent[x].prio = H => ns.sent[x].key = Leader
    PROVE  IndInv'
<1> DEFINE NR == IF ns.step # pst[q].step THEN RequestsOf(q, ns) ELSE {}
<1>1. /\ \A p \in Proposers : GotOK(pst[p]) /\ DecOK(pst[p]) /\ SentOK(p, pst[p])
      /\ \A m2 \in Reqs : Supported(m2.step, m2.prop)
  BY InvForms
<1>2. pst \in [Proposers -> PstT] /\ msgs \subseteq ReqMsg \cup RepMsg BY DEF IndInv, TypeInv, PstT
<1>3. pst' \in [Proposers -> PstT] /\ pst'[q] = ns /\ \A p \in Proposers \ {q} : pst'[p] = pst[p]
  BY <1>2
<1>4. \A r \in NR : r = [type |-> "req", from |-> q, to |-> r.to, step |-> ns.step,
                         prop |-> ns.sent[r.to]] /\ r.to \in Recorders /\ ns.step # pst[q].step
  BY DEF RequestsOf
<1>5. Reqs' = Reqs \cup NR /\ Reps' = Reps \ {m} BY <1>4 DEF Reqs, Reps
<1>6. \A p \in Proposers : pst'[p].step >= pst[p].step BY <1>2, <1>3 DEF PstT
<1>a. TypeInv'
  <2>1. NR \subseteq ReqMsg BY <1>4, <1>3, <1>2 DEF ReqMsg, PstT
  <2> QED BY <1>3, <2>1 DEF IndInv, TypeInv, PstT
<1>b. RegInv' /\ SeenInv'
  <2>1. RegInv' BY DEF IndInv, RegInv, RecAt, Entered, SeenAt, FirstAt, PrevAt, EntryOf
  <2>2. SeenInv' BY <1>5 DEF IndInv, SeenInv, RecAt, Entered, SeenAt
  <2> QED BY <2>1, <2>2
<1>c. ReqInv'
  <2> SUFFICES ASSUME NEW m2 \in Reqs'
               PROVE  /\ m2.step >= FirstStep
                      /\ m2.step <= pst'[m2.from].step
                      /\ m2.step = pst'[m2.from].step => m2.prop = pst'[m2.from].sent[m2.to]
    BY DEF ReqInv
  <2>1. CASE m2 \in Reqs
    <3>1. /\ m2.step >= FirstStep /\ m2.step <= pst[m2.from].step
          /\ m2.step = pst[m2.from].step => m2.prop = pst[m2.from].sent[m2.to]
          /\ m2.from \in Proposers /\ m2.step \in Nat
      BY <2>1, ReqTyped DEF IndInv, ReqInv
    <3>2. CASE m2.from = q
      <4>1. ns.step # pst[q].step => m2.step < ns.step BY <3>1, <3>2, <1>2 DEF PstT
      <4> QED BY <3>1, <3>2, <4>1, <1>3, <1>6
    <3>3. CASE m2.from # q BY <3>1, <3>3, <1>3
    <3> QED BY <3>2, <3>3
  <2>2. CASE m2 \in NR
    <3>1. ns.step >= FirstStep BY DEF SentOK
    <3>2. m2.from = q /\ m2.step = ns.step /\ m2.prop = ns.sent[m2.to] BY <2>2, <1>4
    <3> QED BY <3>1, <3>2, <1>3 DEF PstT
  <2> QED BY <1>5, <2>1, <2>2
<1>d. SentInv'
  <2> SUFFICES ASSUME NEW p \in Proposers
               PROVE  /\ SentOK(p, pst'[p])
                      /\ \A x \in Recorders :
                             [type |-> "req", from |-> p, to |-> x, step |-> pst'[p].step,
                              prop |-> pst'[p].sent[x]] \in msgs'
    BY DEF SentInv, SentOK
  <2>1. \A x \in Recorders :
            [type |-> "req", from |-> p, to |-> x, step |-> pst[p].step,
             prop |-> pst[p].sent[x]] \in msgs
    BY DEF IndInv, SentInv
  <2>2. CASE p = q /\ ns.step # pst[q].step
    <3>1. \A x \in Recorders :
              [type |-> "req", from |-> q, to |-> x, step |-> ns.step, prop |-> ns.sent[x]] \in NR
      BY <2>2 DEF RequestsOf
    <3> QED BY <2>2, <3>1, <1>3
  <2>3. CASE p = q /\ ns.step = pst[q].step
    <3>1. \A x \in Recorders :
              [type |-> "req", from |-> q, to |-> x, step |-> pst[q].step,
               prop |-> pst[q].sent[x]] # m
      BY DEF Reps
    <3> QED BY <2>1, <2>3, <3>1, <1>3
  <2>4. CASE p # q
    <3>1. \A x \in Recorders :
              [type |-> "req", from |-> p, to |-> x, step |-> pst[p].step,
               prop |-> pst[p].sent[x]] # m
      BY DEF Reps
    <3> QED BY <1>1, <2>1, <2>4, <3>1, <1>3
  <2> QED BY <2>2, <2>3, <2>4
<1>e. RepInv'
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
  <2>1. r \in Reps BY <1>5
  <2>2. /\ Entered(r.from, r.step) /\ FirstAt(r.from, r.step) = r.first
        /\ PrevAt(r.from, r.step) = r.prev /\ r.req >= FirstStep /\ r.step >= r.req
        /\ r.req <= pst[r.to].step
        /\ r.req = pst[r.to].step /\ r.step = r.req =>
               pst[r.to].sent[r.from] \in RecAt(r.from, r.step)
        /\ r.to \in Proposers /\ r.req \in Nat
    <3>1. r \in RepMsg BY <2>1, RepTyped
    <3> QED BY <2>1, <3>1 DEF IndInv, RepInv, RepMsg
  <2>3. \A y, u : /\ RecAt(y, u)' = RecAt(y, u) /\ Entered(y, u)' = Entered(y, u)
                  /\ FirstAt(y, u)' = FirstAt(y, u) /\ PrevAt(y, u)' = PrevAt(y, u)
    BY DEF RecAt, Entered, SeenAt, FirstAt, PrevAt, EntryOf
  <2>4. CASE r.to = q
    <3>1. ns.step # pst[q].step => r.req < ns.step BY <2>2, <2>4, <1>2 DEF PstT
    <3> QED BY <2>2, <2>3, <2>4, <3>1, <1>3, <1>6
  <2>5. CASE r.to # q BY <2>2, <2>3, <2>5, <1>3
  <2> QED BY <2>4, <2>5
<1>f. GotInv' /\ DecInv'
  \* GotOK(pst[p])' is GotOK of the new state against the (unchanged) history.
  <2>1. \A p \in Proposers : GotOK(pst[p])' /\ DecOK(pst[p])'
    <3> SUFFICES ASSUME NEW p \in Proposers PROVE GotOK(pst[p])' /\ DecOK(pst[p])' OBVIOUS
    <3>0. PICK sp : sp = pst'[p] OBVIOUS
    <3>1. GotOK(sp) /\ DecOK(sp) BY <1>1, <1>3, <3>0
    <3>2. GotOK(sp)' /\ DecOK(sp)'
      BY <3>1 DEF GotOK, DecOK, QS, RS, RecAt, Entered, SeenAt, FirstAt, PrevAt, EntryOf
    <3> QED BY <3>0, <3>2 DEF GotOK, DecOK, QS, RS
  <2> QED BY <2>1 DEF GotInv, DecInv, GotOK, DecOK, QS, RS, GotQ, Got
<1>g. HInv'
  <2> SUFFICES ASSUME NEW m2 \in Reqs', m2.prop.prio = H PROVE m2.prop.key = Leader BY DEF HInv
  <2>1. CASE m2 \in Reqs BY <2>1 DEF IndInv, HInv
  <2>2. CASE m2 \in NR BY <2>2, <1>4
  <2> QED BY <1>5, <2>1, <2>2
<1>h. SupportInv'
  <2> SUFFICES \A m2 \in Reqs' : Supported(m2.step, m2.prop)' BY DEF SupportInv, Supported
  <2>0. Reqs \subseteq Reqs' BY <1>5
  <2> SUFFICES ASSUME NEW m2 \in Reqs' PROVE Supported(m2.step, m2.prop)' OBVIOUS
  <2>1. Supported(m2.step, m2.prop)
    <3>1. CASE m2 \in Reqs BY <3>1, <1>1
    <3>2. CASE m2 \in NR BY <3>2, <1>4
    <3> QED BY <1>5, <3>1, <3>2
  <2> QED BY <2>0, <2>1, SupportedMono
<1> QED BY <1>a, <1>b, <1>c, <1>d, <1>e, <1>f, <1>g, <1>h DEF IndInv
\* What a draw for step st is: drawn priorities with cur's key, or cur itself.
LEMMA DrawFacts ==
    ASSUME NEW q \in Proposers, NEW st, NEW cur \in Proposal, NEW d \in Draws(q, st, cur)
    PROVE  /\ d \in [Recorders -> Proposal]
           /\ Drawing(q, st) => \A x \in Recorders : d[x].key = cur.key /\ d[x].prio \in Prios
           /\ ~Drawing(q, st) => d = [x \in Recorders |-> cur]
<1>1. CASE Drawing(q, st)
  <2>1. PICK f \in [Recorders -> Prios] : d = [r \in Recorders |-> [prio |-> f[r], key |-> cur.key]]
    BY <1>1 DEF Draws, DrawPool, Drawing
  <2> QED BY <1>1, <2>1, ProposalFields DEF Proposal
<1>2. CASE ~Drawing(q, st)
  <2>1. d = [x \in Recorders |-> cur] BY <1>2 DEF Draws, DrawPool, Drawing
  <2> QED BY <1>2, <2>1
<1> QED BY <1>1, <1>2

\* The proposer moves to step st with proposal cur (or stays, past MaxStep).
LEMMA AdvanceStep ==
    ASSUME IndInv, NEW m \in Reps, NEW q \in Proposers, NEW s, s = pst[q],
           NEW st \in Nat, st > s.step, NEW cur \in Proposal, NEW t \in Advance(q, s, st, cur),
           s.decided = Nil,
           UNCHANGED <<reg, rec, seen>>, pst' = [pst EXCEPT ![q] = t],
           msgs' = (msgs \ {m}) \cup (IF t.step # s.step THEN RequestsOf(q, t) ELSE {}),
           Supported(st, cur),
           cur.prio = H => cur.key = Leader
    PROVE  IndInv'
<1>1. /\ GotOK(s) /\ DecOK(s) /\ SentOK(q, s) /\ s \in PstT
  BY InvForms DEF IndInv, TypeInv, PstT
<1>2. CASE st > MaxStep
  <2>1. t = s BY <1>2 DEF Advance
  <2> QED BY <1>1, <2>1, ProposerStep DEF PstT
<1>3. CASE st <= MaxStep
  <2>1. PICK d \in Draws(q, st, cur) :
            t = [s EXCEPT !.step = st, !.p = cur, !.sent = d, !.replies = [r \in Recorders |-> {}]]
    BY <1>3, Assumptions DEF Advance
  <2>2. /\ d \in [Recorders -> Proposal]
        /\ Drawing(q, st) => \A x \in Recorders : d[x].key = cur.key /\ d[x].prio \in Prios
        /\ ~Drawing(q, st) => d = [x \in Recorders |-> cur]
    BY DrawFacts
  <2>3. /\ t.step = st /\ t.p = cur /\ t.sent = d /\ t.replies = [r \in Recorders |-> {}]
        /\ t.decided = Nil
    BY <2>1, <1>1 DEF PstT
  <2>4. t \in PstT BY <2>1, <2>2, <1>1 DEF PstT
  <2>5. GotOK(t) /\ DecOK(t) BY <2>3 DEF GotOK, DecOK
  <2>6. SentOK(q, t)
    <3>1. t.step >= FirstStep BY <2>3, <1>1 DEF SentOK, PstT, FirstStep
    <3>2. Drawing(q, t.step) = Drawing(q, st) BY <2>3
    <3> QED BY <2>2, <2>3, <3>1, <3>2 DEF SentOK
  <2>7. \A x \in Recorders : /\ Supported(st, d[x])
                             /\ d[x].prio = H => d[x].key = Leader
    <3> SUFFICES ASSUME NEW x \in Recorders
                 PROVE  Supported(st, d[x]) /\ (d[x].prio = H => d[x].key = Leader)
      OBVIOUS
    <3>1. CASE Drawing(q, st)
      <4>1. d[x].key = cur.key /\ d[x].prio \in Prios BY <2>2, <3>1
      <4>2. d[x].prio # H BY <4>1, Assumptions DEF H
      <4>3. st % 4 = 0 BY <3>1 DEF Drawing
      <4> QED BY <4>1, <4>2, <4>3, SupportedKey
    <3>2. CASE ~Drawing(q, st) BY <2>2, <3>2
    <3> QED BY <3>1, <3>2
  <2>8. t.step # s.step BY <2>3, <1>1 DEF PstT
  <2> QED BY <2>3, <2>4, <2>5, <2>6, <2>7, <2>8, <1>1, ProposerStep DEF PstT
<1> QED BY <1>2, <1>3, Assumptions
\* The proposer stays at its step (recording a reply, or deciding).
LEMMA SameStep ==
    ASSUME IndInv, NEW m \in Reps, m.to \in Proposers, NEW s, s = pst[m.to], NEW ns,
           UNCHANGED <<reg, rec, seen>>,
           pst' = [pst EXCEPT ![m.to] = ns],
           msgs' = msgs \ {m},
           ns \in PstT, ns.step = s.step, ns.sent = s.sent, ns.p = s.p,
           GotOK(ns), DecOK(ns), SentOK(m.to, ns)
    PROVE  IndInv'
<1>1. s \in PstT BY DEF IndInv, TypeInv, PstT
<1>2. msgs' = (msgs \ {m}) \cup (IF ns.step # pst[m.to].step THEN RequestsOf(m.to, ns) ELSE {})
  OBVIOUS
<1> QED BY <1>1, <1>2, ProposerStep DEF PstT

-----------------------------------------------------------------------------
(* A proposer handles a reply *)

LEMMA DeliverStep ==
    ASSUME IndInv, NEW m \in msgs, Deliver(m), UNCHANGED <<rec, seen>>
    PROVE  IndInv'
<1>0. PICK s : s = pst[m.to] OBVIOUS
<1> DEFINE q == m.to
           got == [s.replies EXCEPT ![m.from] = {m}]
           reps == UNION {got[r] : r \in Recorders}
           firsts == {x.first : x \in reps}
           prevs == {x.prev : x \in reps} \ {Nil}
           best == IF prevs = {} THEN Nil ELSE Max(prevs)
           QG == {r \in Recorders : got[r] # {}}
           G == [s EXCEPT !.replies = got]
<1>1. /\ m \in Reps /\ m \in RepMsg /\ UNCHANGED reg
      /\ msgs' = (msgs \ {m}) \cup (IF pst'[q].step # s.step THEN RequestsOf(q, pst'[q]) ELSE {})
  BY <1>0, RepTyped DEF Deliver, OnReply, Reps
<1>2. /\ q \in Proposers /\ m.from \in Recorders /\ m.req \in Nat /\ m.step \in Nat
      /\ m.first \in Proposal \cup {Nil} /\ m.prev \in Proposal \cup {Nil}
  BY <1>1 DEF RepMsg
<1>3. /\ Entered(m.from, m.step) /\ FirstAt(m.from, m.step) = m.first
      /\ PrevAt(m.from, m.step) = m.prev
      /\ m.step >= m.req /\ m.req <= s.step
      /\ m.req = s.step /\ m.step = m.req => s.sent[m.from] \in RecAt(m.from, m.step)
  BY <1>0, <1>1 DEF IndInv, RepInv
<1>4. /\ s \in PstT /\ GotOK(s) /\ DecOK(s) /\ SentOK(q, s)
  BY <1>0, <1>2, InvForms DEF IndInv, TypeInv, PstT
<1>5. pst \in [Proposers -> PstT] BY DEF IndInv, TypeInv, PstT
<1>6. /\ s.step \in Nat /\ s.step >= FirstStep /\ s.p \in Proposal
      /\ s.sent \in [Recorders -> Proposal] /\ s.replies \in [Recorders -> SUBSET RepMsg]
  BY <1>4 DEF PstT, SentOK
\* Staying put.
<1>7. pst' = pst => IndInv'
  <2> SUFFICES ASSUME pst' = pst PROVE IndInv' OBVIOUS
  <2>1. pst' = [pst EXCEPT ![q] = s] BY <1>0, <1>2, <1>5
  <2>2. msgs' = msgs \ {m} BY <1>0, <1>1
  <2>3. UNCHANGED <<reg, rec, seen>> BY <1>1
  <2> QED BY <1>0, <1>1, <1>2, <1>4, <2>1, <2>2, <2>3, SameStep
\* At a step other than a draw, the proposer's own proposal is one of its requests.
<1>8. ~Drawing(q, s.step) => \E m0 \in Reqs : m0.step = s.step /\ m0.prop = s.p
  <2> SUFFICES ASSUME ~Drawing(q, s.step) PROVE \E m0 \in Reqs : m0.step = s.step /\ m0.prop = s.p
    OBVIOUS
  <2>1. PICK x \in Recorders : TRUE BY Assumptions
  <2>2. [type |-> "req", from |-> q, to |-> x, step |-> s.step, prop |-> s.sent[x]] \in msgs
    BY <1>0, <1>2 DEF IndInv, SentInv
  <2>3. s.sent[x] = s.p BY <1>4 DEF SentOK
  <2> QED BY <2>2, <2>3 DEF Reqs
<1>9. CASE s.decided # Nil \/ m.req # s.step
  <2>1. pst' = pst BY <1>0, <1>9 DEF Deliver, OnReply
  <2> QED BY <1>7, <2>1
<1>10. CASE s.decided = Nil /\ m.req = s.step /\ m.step > s.step /\ m.first = Nil
  <2>1. pst' = pst BY <1>0, <1>10 DEF Deliver, OnReply
  <2> QED BY <1>7, <2>1
\* Catch-up: adopt the recorder's first proposal at its later step.
<1>11. CASE s.decided = Nil /\ m.req = s.step /\ m.step > s.step /\ m.first # Nil
  <2>1. PICK t \in Advance(q, s, m.step, m.first) : pst' = [pst EXCEPT ![q] = t]
    BY <1>0, <1>11 DEF Deliver, OnReply
  <2>2. m.first \in RecAt(m.from, m.step) /\ m.first \in Proposal BY <1>2, <1>3, EntryFacts, RecProps
  <2>3. PICK m0 \in Reqs : m0.step = m.step /\ m0.prop = m.first BY <2>2, RecProps, ReqPropsTyped
  <2>4. Supported(m.step, m.first) BY <2>3, InvForms
  <2>5. m.first.prio = H => m.first.key = Leader BY <2>3 DEF IndInv, HInv
  <2>6. msgs' = (msgs \ {m}) \cup (IF t.step # s.step THEN RequestsOf(q, t) ELSE {})
    BY <1>1, <2>1, <1>2, <1>5
  <2> QED BY <1>0, <1>1, <1>2, <1>11, <2>1, <2>2, <2>4, <2>5, <2>6, AdvanceStep
<1>12. CASE s.decided = Nil /\ m.req = s.step /\ m.step = s.step
  <2>2. /\ G \in PstT /\ G.step = s.step /\ G.sent = s.sent /\ G.p = s.p
        /\ G.replies = got /\ G.decided = Nil
    <3>1. got \in [Recorders -> SUBSET RepMsg] BY <1>1, <1>6
    <3> QED BY <3>1, <1>4, <1>12 DEF PstT
  \* Every reply now counted is for this step and recorded the request.
  <2>0. \A y \in Recorders : \A r \in got[y] :
            /\ r.from = y /\ r.req = s.step /\ r.step = s.step
            /\ Entered(y, r.step) /\ FirstAt(y, r.step) = r.first
            /\ PrevAt(y, r.step) = r.prev
            /\ s.sent[y] \in RecAt(y, r.step)
    <3> SUFFICES ASSUME NEW y \in Recorders, NEW r \in got[y]
                 PROVE  /\ r.from = y /\ r.req = s.step /\ r.step = s.step
                        /\ Entered(y, r.step) /\ FirstAt(y, r.step) = r.first
                        /\ PrevAt(y, r.step) = r.prev
                        /\ s.sent[y] \in RecAt(y, r.step)
      OBVIOUS
    <3>1. CASE y = m.from
      <4>1. r = m BY <3>1, <1>6
      <4> QED BY <4>1, <3>1, <1>3, <1>12, <1>1 DEF Reps
    <3>2. CASE y # m.from
      <4>1. r \in s.replies[y] BY <3>2, <1>6
      <4> QED BY <4>1, <1>4 DEF GotOK
    <3> QED BY <3>1, <3>2
  <2>1. GotOK(G) BY <2>0, <2>2 DEF GotOK
  <2>3. \A y \in QG : \E r \in got[y] : TRUE BY DEF QG
  <2>4. \A y \in QG : Entered(y, s.step) /\ FirstAt(y, s.step) \in RecAt(y, s.step)
                      /\ s.sent[y] \in RecAt(y, s.step)
    <3> SUFFICES ASSUME NEW y \in QG
                 PROVE  Entered(y, s.step) /\ FirstAt(y, s.step) \in RecAt(y, s.step)
                        /\ s.sent[y] \in RecAt(y, s.step)
      OBVIOUS
    <3>1. PICK r \in got[y] : TRUE BY <2>3
    <3>2. Entered(y, s.step) /\ s.sent[y] \in RecAt(y, s.step) BY <3>1, <2>0
    <3> QED BY <3>2, EntryFacts
  <2>5. firsts = {FirstAt(y, s.step) : y \in QG}
    <3>1. \A x \in reps : \E y \in QG : x.first = FirstAt(y, s.step) BY <2>0
    <3>2. \A y \in QG : \E x \in reps : x.first = FirstAt(y, s.step) BY <2>0, <2>3
    <3> QED BY <3>1, <3>2
  <2>6. prevs = {PrevAt(y, s.step) : y \in QG} \ {Nil}
    <3>1. \A x \in reps : \E y \in QG : x.prev = PrevAt(y, s.step) BY <2>0
    <3>2. \A y \in QG : \E x \in reps : x.prev = PrevAt(y, s.step) BY <2>0, <2>3
    <3> QED BY <3>1, <3>2
  <2>c. Cardinality(QG) \in Nat /\ Quorum \in Nat
    <3>1. IsFiniteSet(QG) BY Assumptions, FS_Subset
    <3> QED BY <3>1, Assumptions, FS_CardinalityType DEF Quorum
  <2>7. firsts \subseteq Proposal BY <2>4, <2>5, RecProps
  <2>8. CASE Cardinality(QG) < Quorum
    <3>0. pst' = [pst EXCEPT ![q].replies = got] BY <1>0, <1>12, <2>8 DEF Deliver, OnReply
    <3>1. pst' = [pst EXCEPT ![q] = G] BY <3>0, <1>0, <1>2, <1>5
    <3>2. DecOK(G) /\ SentOK(q, G) BY <2>2, <1>4 DEF DecOK, SentOK
    <3>3. msgs' = msgs \ {m} BY <1>1, <3>1, <1>2, <1>5, <2>2
    <3>4. UNCHANGED <<reg, rec, seen>> BY <1>1
    <3> QED BY <1>0, <1>1, <1>2, <2>1, <2>2, <3>1, <3>2, <3>3, <3>4, SameStep
  <2>9. CASE Cardinality(QG) >= Quorum
    <3>0. QG \in Quorums /\ QG # {}
      <4>1. QG \in Quorums BY <2>9 DEF Quorums
      <4>2. QG # {} BY <4>1, QuorumsIntersect
      <4> QED BY <4>1, <4>2
    <3>a. firsts # {} BY <3>0, <2>5
    <3>b. Nil \notin firsts BY <2>7, ProposalFields
    <3>c. prevs \subseteq Proposal
      <4> SUFFICES ASSUME NEW y \in QG, PrevAt(y, s.step) # Nil PROVE PrevAt(y, s.step) \in Proposal
        BY <2>6
      <4>1. RecAt(y, s.step - 1) # {} BY <2>4, PrevFacts
      <4> QED BY <2>4, <4>1, PrevFacts
    \* A `prev` at this step is a proposal of the step before.
    <3>d. \A w \in prevs : \E m0 \in Reqs : m0.prop = w
      <4> SUFFICES ASSUME NEW y \in QG, PrevAt(y, s.step) # Nil
                   PROVE  \E m0 \in Reqs : m0.prop = PrevAt(y, s.step)
        BY <2>6
      <4>1. RecAt(y, s.step - 1) # {} BY <2>4, PrevFacts
      <4>2. PrevAt(y, s.step) \in ReqProps(s.step - 1) BY <2>4, <4>1, PrevFacts, RecProps
      <4> QED BY <4>2, ReqPropsTyped
    <3>e. \A f \in firsts : \E m0 \in Reqs : m0.step = s.step /\ m0.prop = f
      BY <2>4, <2>5, RecProps, ReqPropsTyped
    <3>f. (s.step + 1) - 1 = s.step /\ s.step + 1 \in Nat /\ s.step + 1 > s.step BY <1>6
    <3>g. ~(Cardinality(QG) < Quorum) BY <2>9, <2>c
    \* The cases of OnReply once a quorum answered.
    <3>1. CASE s.step % 4 = 0 /\ s.step = FirstStep /\ \A f \in firsts : f # Nil /\ f.prio = H
      <4> DEFINE D == [G EXCEPT !.decided = CHOOSE f \in firsts : TRUE]
      <4>0. pst' = [pst EXCEPT ![q].replies = got, ![q].decided = CHOOSE f \in firsts : TRUE]
        BY <1>0, <1>12, <3>g, <3>1 DEF Deliver, OnReply
      <4>1. pst' = [pst EXCEPT ![q] = D] BY <4>0, <1>0, <1>2, <1>5
      <4>2. (CHOOSE f \in firsts : TRUE) \in firsts BY <3>a
      <4>3. /\ D \in PstT /\ D.step = s.step /\ D.sent = s.sent /\ D.p = s.p /\ D.replies = got
            /\ D.decided \in firsts
        BY <2>2, <2>7, <4>2 DEF PstT
      <4>4. GotOK(D) BY <2>0, <4>3 DEF GotOK
      <4>5. DecOK(D)
        <5>1. QS(D) = QG /\ RS(D) = reps BY <4>3 DEF QS, RS
        <5> QED BY <5>1, <3>0, <3>1, <4>3 DEF DecOK
      <4>6. SentOK(q, D) BY <4>3, <1>4 DEF SentOK
      <4>7. msgs' = msgs \ {m} BY <1>1, <4>1, <1>2, <1>5, <4>3
      <4>8. UNCHANGED <<reg, rec, seen>> BY <1>1
      <4> QED BY <1>0, <1>1, <1>2, <4>1, <4>3, <4>4, <4>5, <4>6, <4>7, <4>8, SameStep
    <3>2. CASE s.step % 4 = 0 /\ ~(s.step = FirstStep /\ \A f \in firsts : f # Nil /\ f.prio = H)
      <4> DEFINE cur == Max(firsts)
      <4>1. firsts \ {Nil} = firsts BY <3>b
      <4>a. (IF firsts \ {Nil} = {} THEN s.p ELSE Max(firsts \ {Nil})) = cur BY <4>1, <3>a
      <4>b. \E t \in Advance(q, s, s.step + 1,
                             IF firsts \ {Nil} = {} THEN s.p ELSE Max(firsts \ {Nil})) :
                pst' = [pst EXCEPT ![q] = t]
        BY <1>0, <1>12, <3>g, <3>2 DEF Deliver, OnReply
      <4>2. PICK t \in Advance(q, s, s.step + 1, cur) : pst' = [pst EXCEPT ![q] = t]
        BY <4>a, <4>b
      <4>3. cur \in firsts /\ \A f \in firsts : Leq(f, cur) BY <2>7, <3>a, MaxProps
      <4>4. Supported(s.step + 1, cur)
        <5>1. (s.step + 1) % 4 = 1 BY <3>2, <1>6
        <5>2. \E y \in QG : FirstAt(y, s.step) = cur BY <4>3, <2>5
        <5>3. \A y \in QG : Leq(FirstAt(y, s.step), cur) BY <4>3, <2>5
        <5> QED BY <5>1, <5>2, <5>3, <3>0, <3>f, <2>4 DEF Supported
      <4>5. cur.prio = H => cur.key = Leader BY <4>3, <3>e DEF IndInv, HInv
      <4>6. msgs' = (msgs \ {m}) \cup (IF t.step # s.step THEN RequestsOf(q, t) ELSE {})
        BY <1>1, <4>2, <1>2, <1>5
      <4> QED BY <1>0, <1>1, <1>2, <1>12, <2>7, <3>f, <4>2, <4>3, <4>4, <4>5, <4>6, AdvanceStep
    <3>3. CASE s.step % 4 = 2 /\ best = s.p
      <4> DEFINE D == [G EXCEPT !.decided = s.p]
      <4>0. pst' = [pst EXCEPT ![q].replies = got, ![q].decided = s.p]
        BY <1>0, <1>12, <3>g, <3>3 DEF Deliver, OnReply
      <4>1. pst' = [pst EXCEPT ![q] = D] BY <4>0, <1>0, <1>2, <1>5
      <4>2. /\ D \in PstT /\ D.step = s.step /\ D.sent = s.sent /\ D.p = s.p /\ D.replies = got
            /\ D.decided = s.p
        BY <2>2, <1>6 DEF PstT
      <4>3. prevs # {} /\ Max(prevs) = s.p BY <3>3, <1>6, ProposalFields
      <4>4. Max(prevs) \in prevs /\ \A o \in prevs : Leq(o, Max(prevs)) BY <3>c, <4>3, MaxProps
      <4>5. DecOK(D)
        <5>1. QS(D) = QG /\ RS(D) = reps BY <4>2 DEF QS, RS
        <5>2. \A r \in reps : r.prev # Nil => Leq(r.prev, s.p) BY <4>3, <4>4
        <5>3. \E r \in reps : r.prev = s.p BY <4>3, <4>4
        <5> QED BY <5>1, <5>2, <5>3, <3>0, <3>3, <4>2 DEF DecOK
      <4>6. GotOK(D) /\ SentOK(q, D) BY <2>0, <4>2, <1>4 DEF GotOK, SentOK
      <4>7. msgs' = msgs \ {m} BY <1>1, <4>1, <1>2, <1>5, <4>2
      <4>8. UNCHANGED <<reg, rec, seen>> BY <1>1
      <4> QED BY <1>0, <1>1, <1>2, <4>1, <4>2, <4>5, <4>6, <4>7, <4>8, SameStep
    \* Phases 1 and 2 (no decision) carry the proposal on.
    <3>4. CASE \/ s.step % 4 = 1
               \/ s.step % 4 = 2 /\ best # s.p
      <4>1. PICK t \in Advance(q, s, s.step + 1, s.p) : pst' = [pst EXCEPT ![q] = t]
        BY <1>0, <1>12, <3>g, <3>4 DEF Deliver, OnReply
      <4>2. ~Drawing(q, s.step) BY <3>4 DEF Drawing
      <4>3. PICK m0 \in Reqs : m0.step = s.step /\ m0.prop = s.p BY <1>8, <4>2
      <4>4. s.p \in ReqProps(s.step) BY <4>3 DEF ReqProps
      <4>5. s.sent = [x \in Recorders |-> s.p] BY <4>2, <1>4 DEF SentOK
      <4>6. Supported(s.step + 1, s.p)
        <5>1. CASE s.step % 4 = 1
          <6>1. (s.step + 1) % 4 = 2 BY <5>1, <1>6
          <6>2. \A y \in QG : \E u \in RecAt(y, s.step) : Leq(s.p, u)
            BY <2>4, <4>5, <1>6, Order
          <6> QED BY <6>1, <6>2, <4>4, <3>0, <3>f DEF Supported
        <5>2. CASE s.step % 4 = 2
          <6>1. (s.step + 1) % 4 = 3 BY <5>2, <1>6
          <6> QED BY <6>1, <4>4, <3>f DEF Supported
        <5> QED BY <3>4, <5>1, <5>2
      <4>7. s.p.prio = H => s.p.key = Leader BY <4>3 DEF IndInv, HInv
      <4>8. msgs' = (msgs \ {m}) \cup (IF t.step # s.step THEN RequestsOf(q, t) ELSE {})
        BY <1>1, <4>1, <1>2, <1>5
      <4> QED BY <1>0, <1>1, <1>2, <1>6, <1>12, <3>f, <4>1, <4>6, <4>7, <4>8, AdvanceStep
    \* Phase 3 adopts the best `prev`, or keeps its own proposal if there is none.
    <3>5. CASE s.step % 4 = 3
      <4> DEFINE cur == IF best = Nil THEN s.p ELSE best
      <4>1. PICK t \in Advance(q, s, s.step + 1, cur) : pst' = [pst EXCEPT ![q] = t]
        BY <1>0, <1>12, <3>g, <3>5 DEF Deliver, OnReply
      <4>2. ~Drawing(q, s.step) BY <3>5 DEF Drawing
      <4>3. PICK m0 \in Reqs : m0.step = s.step /\ m0.prop = s.p BY <1>8, <4>2
      <4>4. prevs # {} => best = Max(prevs) /\ best \in prevs /\ best \in Proposal
        BY <3>c, MaxProps
      <4>5. prevs = {} => cur = s.p BY ProposalFields, <1>6
      <4>6. prevs # {} => cur = Max(prevs) BY <4>4, ProposalFields
      <4>7. cur \in Proposal BY <4>4, <4>5, <4>6, <1>6
      <4>8. Supported(s.step + 1, cur)
        <5>1. (s.step + 1) % 4 = 0 /\ s.step + 1 > FirstStep BY <3>5, <1>6 DEF FirstStep
        <5>2. {PrevAt(y, (s.step + 1) - 1) : y \in QG} \ {Nil} = prevs BY <2>6, <3>f
        <5>3. prevs = {} => \E w2 \in ReqProps(s.step) : w2.key = cur.key
          BY <4>3, <4>5 DEF ReqProps
        <5> QED BY <5>1, <5>2, <5>3, <4>6, <3>0, <3>f, <2>4 DEF Supported
      <4>9. cur.prio = H => cur.key = Leader
        <5>1. CASE prevs = {} BY <5>1, <4>5, <4>3 DEF IndInv, HInv
        <5>2. CASE prevs # {}
          <6>1. PICK m1 \in Reqs : m1.prop = Max(prevs) BY <5>2, <4>4, <3>d
          <6> QED BY <5>2, <4>6, <6>1 DEF IndInv, HInv
        <5> QED BY <5>1, <5>2
      <4>10. msgs' = (msgs \ {m}) \cup (IF t.step # s.step THEN RequestsOf(q, t) ELSE {})
        BY <1>1, <4>1, <1>2, <1>5
      <4> QED BY <1>0, <1>1, <1>2, <1>12, <3>f, <4>1, <4>7, <4>8, <4>9, <4>10, AdvanceStep
    <3> QED BY <3>1, <3>2, <3>3, <3>4, <3>5, <1>6
  <2> QED BY <2>8, <2>9, <2>c
<1> QED BY <1>2, <1>3, <1>6, <1>9, <1>10, <1>11, <1>12
=============================================================================

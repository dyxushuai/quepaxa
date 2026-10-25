--------------------------- MODULE QuePaxaProof ---------------------------
(***************************************************************************)
(* An inductive invariant for QuePaxa.tla, the base of a proof that every *)
(* decision agrees.                                                        *)
(*                                                                         *)
(* QuePaxa.tla keeps only what a recorder stores: its register for the     *)
(* current step. Safety rests on history (a recorder that answered a step  *)
(* recorded the proposal before it moved on, so its next `prev` covers     *)
(* it), so this module adds two history variables, which only record and   *)
(* never constrain a step of the protocol:                                 *)
(*   rec   every proposal a recorder recorded, with the step it was at;    *)
(*   seen  for each step a recorder entered, its `first` and `prev` there. *)
(*                                                                         *)
(* IndInv states only local facts: registers and replies agree with the    *)
(* history, requests and decisions are what the proposers sent and saw,    *)
(* the top priority H is the leader's, and every proposal at a step is     *)
(* backed by a quorum of the step before ("support"). Agreement is not     *)
(* part of it: it follows from IndInv by induction over steps.             *)
(***************************************************************************)
EXTENDS QuePaxa, Naturals

VARIABLES rec, seen

hvars == <<reg, pst, msgs, rec, seen>>

\* Record(m), and what it adds to the history.
RecordH(m) ==
    LET g == reg[m.to] IN
    /\ Record(m)
    /\ rec' = IF m.step >= g.step
              THEN rec \cup {[r |-> m.to, step |-> m.step, prop |-> m.prop]}
              ELSE rec
    /\ seen' = IF m.step > g.step
               THEN seen \cup {[r |-> m.to, step |-> m.step, first |-> m.prop,
                                prev |-> IF g.step + 1 = m.step THEN g.agg ELSE Nil]}
               ELSE seen

HInit == Init /\ rec = {} /\ seen = {}
HNext == \E m \in msgs : RecordH(m) \/ (Deliver(m) /\ UNCHANGED <<rec, seen>>)
HSpec == HInit /\ [][HNext]_hvars

-----------------------------------------------------------------------------
Quorums == {Q \in SUBSET Recorders : Cardinality(Q) >= Quorum}
Leq(a, b) == ~Gt(a, b)
MaxOf(S) == IF S = {} THEN Nil ELSE Max(S)

Proposal == [prio : Prios \cup {H}, key : Proposers]
ReqMsg == [type : {"req"}, from : Proposers, to : Recorders, step : Nat, prop : Proposal]
RepMsg == [type : {"rep"}, from : Recorders, to : Proposers, req : Nat, step : Nat,
           first : Proposal \cup {Nil}, prev : Proposal \cup {Nil}]

Reqs == {m \in msgs : m.type = "req"}
Reps == {m \in msgs : m.type = "rep"}
ReqProps(t) == {m.prop : m \in {m \in Reqs : m.step = t}}

RecAt(x, t) == {e.prop : e \in {e \in rec : e.r = x /\ e.step = t}}
SeenAt(x, t) == {s \in seen : s.r = x /\ s.step = t}
Entered(x, t) == SeenAt(x, t) # {}
EntryOf(x, t) == CHOOSE s \in SeenAt(x, t) : TRUE
FirstAt(x, t) == EntryOf(x, t).first
PrevAt(x, t) == EntryOf(x, t).prev

\* Draws a fresh priority per recorder at this step.
Drawing(q, t) == t % 4 = 0 /\ (t > FirstStep \/ q # Leader)

GotQ(q) == {x \in Recorders : pst[q].replies[x] # {}}
Got(q) == UNION {pst[q].replies[x] : x \in Recorders}

-----------------------------------------------------------------------------
TypeInv ==
    /\ reg \in [Recorders -> [step : Nat, first : Proposal \cup {Nil},
                              agg : Proposal \cup {Nil}, prev : Proposal \cup {Nil}]]
    /\ pst \in [Proposers -> [step : Nat, p : Proposal, sent : [Recorders -> Proposal],
                              replies : [Recorders -> SUBSET RepMsg],
                              decided : Proposal \cup {Nil}]]
    /\ msgs \subseteq ReqMsg \cup RepMsg
    /\ rec \subseteq [r : Recorders, step : Nat, prop : Proposal]
    /\ seen \subseteq [r : Recorders, step : Nat, first : Proposal, prev : Proposal \cup {Nil}]

\* A register is its history's latest entry; nothing is recorded ahead of it.
RegInv ==
    \A x \in Recorders :
        /\ \A e \in rec : e.r = x => e.step <= reg[x].step
        /\ \A s \in seen : s.r = x => s.step <= reg[x].step
        /\ reg[x].step = 0 =>
               /\ reg[x].first = Nil /\ reg[x].agg = Nil /\ reg[x].prev = Nil
        /\ reg[x].step # 0 =>
               /\ Entered(x, reg[x].step)
               /\ FirstAt(x, reg[x].step) = reg[x].first
               /\ PrevAt(x, reg[x].step) = reg[x].prev
               /\ reg[x].first \in RecAt(x, reg[x].step)
               /\ reg[x].agg = Max(RecAt(x, reg[x].step))

\* One entry per step entered; `prev` is the best recorded at the step before, if any.
SeenInv ==
    /\ \A s \in seen :
           /\ s.step >= FirstStep
           /\ \A s2 \in seen : s2.r = s.r /\ s2.step = s.step => s2 = s
           /\ s.first \in RecAt(s.r, s.step)
           /\ s.prev = MaxOf(RecAt(s.r, s.step - 1))
    /\ \A e \in rec : Entered(e.r, e.step)
    /\ \A e \in rec : \E m \in Reqs : m.to = e.r /\ m.step = e.step /\ m.prop = e.prop

\* Requests are the proposers' own, for the current step or an earlier one.
ReqInv ==
    \A m \in Reqs :
        /\ m.step >= FirstStep
        /\ m.step <= pst[m.from].step
        /\ m.step = pst[m.from].step => m.prop = pst[m.from].sent[m.to]

\* A proposer sent its current step's requests: drawn priorities or its proposal.
SentInv ==
    \A q \in Proposers :
        LET s == pst[q] IN
        /\ s.step >= FirstStep
        /\ Drawing(q, s.step) =>
               \A x \in Recorders : s.sent[x].key = s.p.key /\ s.sent[x].prio \in Prios
        /\ ~Drawing(q, s.step) => s.sent = [x \in Recorders |-> s.p]
        /\ \A x \in Recorders :
               [type |-> "req", from |-> q, to |-> x, step |-> s.step, prop |-> s.sent[x]] \in msgs

\* A reply is the sender's history; one for the current step proves the request recorded.
RepInv ==
    \A m \in Reps :
        /\ Entered(m.from, m.step)
        /\ FirstAt(m.from, m.step) = m.first
        /\ PrevAt(m.from, m.step) = m.prev
        /\ m.req >= FirstStep
        /\ m.step >= m.req
        /\ m.req <= pst[m.to].step
        /\ m.req = pst[m.to].step /\ m.step = m.req =>
               pst[m.to].sent[m.from] \in RecAt(m.from, m.step)

\* Replies a proposer counts are for its current step and recorded its request.
GotInv ==
    \A q \in Proposers, x \in Recorders :
        \A m \in pst[q].replies[x] :
            /\ m.from = x
            /\ m.req = pst[q].step /\ m.step = pst[q].step
            /\ Entered(x, m.step)
            /\ FirstAt(x, m.step) = m.first
            /\ PrevAt(x, m.step) = m.prev
            /\ pst[q].sent[x] \in RecAt(x, m.step)

\* A decision is a quorum's answer: the best `prev` at phase 2, or H everywhere at step 4.
DecInv ==
    \A q \in Proposers :
        pst[q].decided # Nil =>
            /\ GotQ(q) \in Quorums
            /\ \/ /\ pst[q].step % 4 = 2
                  /\ pst[q].decided = pst[q].p
                  /\ \A m \in Got(q) : m.prev # Nil => Leq(m.prev, pst[q].p)
                  /\ \E m \in Got(q) : m.prev = pst[q].p
               \/ /\ pst[q].step = FirstStep
                  /\ \A m \in Got(q) : m.first # Nil /\ m.first.prio = H
                  /\ \E m \in Got(q) : m.first = pst[q].decided

\* G5: only the leader's value carries the top priority.
HInv == \A m \in Reqs : m.prop.prio = H => m.prop.key = Leader

\* Support: every proposal at a step is backed by a quorum of the step before.
SupportInv ==
    \A m \in Reqs :
        /\ m.step % 4 = 1 =>
               \E Q \in Quorums :
                   /\ \A y \in Q : Entered(y, m.step - 1)
                   /\ \E y \in Q : FirstAt(y, m.step - 1) = m.prop
                   /\ \A y \in Q : Leq(FirstAt(y, m.step - 1), m.prop)
        /\ m.step % 4 = 2 =>
               /\ m.prop \in ReqProps(m.step - 1)
               /\ \E Q \in Quorums : \A y \in Q : \E u \in RecAt(y, m.step - 1) : Leq(m.prop, u)
        /\ m.step % 4 = 3 => m.prop \in ReqProps(m.step - 1)
        /\ m.step % 4 = 0 /\ m.step > FirstStep =>
               \E Q \in Quorums :
                   /\ \A y \in Q : Entered(y, m.step - 1)
                   /\ LET P == {PrevAt(y, m.step - 1) : y \in Q} \ {Nil}
                      IN IF P # {} THEN m.prop.key = Max(P).key
                         ELSE \E w \in ReqProps(m.step - 1) : w.key = m.prop.key

IndInv ==
    /\ TypeInv
    /\ RegInv
    /\ SeenInv
    /\ ReqInv
    /\ SentInv
    /\ RepInv
    /\ GotInv
    /\ DecInv
    /\ HInv
    /\ SupportInv
=============================================================================

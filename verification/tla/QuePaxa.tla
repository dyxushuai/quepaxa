---------------------------- MODULE QuePaxa ----------------------------
(***************************************************************************)
(* One slot of QuePaxa (SOSP'23, Algorithms 3 and 4) as implemented in    *)
(* crates/quepaxa/src/recorder.rs and proposer.rs, written independently  *)
(* of the Rust code's tests: recorders hold an interval summary register  *)
(* (S, Fc, Ac, Ap); proposers walk steps 4r+phase; the leader may use the *)
(* top priority H at the first step; everyone else draws priorities.      *)
(*                                                                         *)
(* The network delivers messages in any order, or never (loss, reordering).*)
(* Requests stay in it and may be delivered again at any time: proposers   *)
(* retransmit unanswered requests, and a recorder answers every copy with  *)
(* its current summary, which may be newer. A reply is delivered at most   *)
(* once: copies of one reply are identical, so a copy adds nothing.        *)
(* A proposer sends its requests for a step when it enters the step; with  *)
(* delivery arbitrary, sending them one by one adds only interleavings.    *)
(* Random priorities are a nondeterministic choice per recorder.           *)
(*                                                                         *)
(* Not modelled (covered by the Rust simulator and model checker):         *)
(* crash-recovery and durability, multiple slots and pipelining, leader    *)
(* rotation, membership changes. Too large to exhaust: run.sh uses TLC's  *)
(* simulation mode and checks that two seeded bugs are still found.        *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, TLC

CONSTANTS Recorders,   \* model values, e.g. {r1, r2, r3}
          Proposers,   \* e.g. 1..2 (a proposer's id is also its value's key)
          Leader,      \* the proposer allowed H at the first step
          Prios,       \* priorities a proposer may draw, all below H
          MaxStep      \* steps explored (4 = round 1 phase 0)

H == 1000
FirstStep == 4
Nil == [prio |-> 0, key |-> 0]          \* no proposal (sorts below every proposal)
Quorum == Cardinality(Recorders) \div 2 + 1

\* Proposals are ordered by priority, then by key (keys are unique per value).
Gt(a, b) == a.prio > b.prio \/ (a.prio = b.prio /\ a.key > b.key)
Max(S) == CHOOSE m \in S : \A o \in S : ~Gt(o, m)

VARIABLES reg,      \* reg[r] = [step, first, agg, prev]
          pst,      \* pst[p] = [step, p, sent, replies, decided]
          msgs      \* requests and replies ever sent

vars == <<reg, pst, msgs>>

(* The proposals sent at `step`: drawn per recorder at phase 0 (except the leader at the
   first step, which keeps H), else the working proposal. *)
Draws(p, step, cur) ==
    IF step % 4 = 0 /\ (step > FirstStep \/ p # Leader)
    THEN {[r \in Recorders |-> [prio |-> f[r], key |-> cur.key]] : f \in [Recorders -> Prios]}
    ELSE {[r \in Recorders |-> cur]}

RequestsOf(p, s) == {[type |-> "req", from |-> p, to |-> r, step |-> s.step, prop |-> s.sent[r]] :
                       r \in Recorders}

Init ==
    /\ reg = [r \in Recorders |-> [step |-> 0, first |-> Nil, agg |-> Nil, prev |-> Nil]]
    /\ \E sent \in [Proposers -> [Recorders -> [prio : Prios \cup {H}, key : Proposers]]] :
         /\ \A p \in Proposers : sent[p] \in Draws(p, FirstStep, [prio |-> H, key |-> p])
         /\ pst = [p \in Proposers |-> [step |-> FirstStep, p |-> [prio |-> H, key |-> p],
                     sent |-> sent[p], replies |-> [r \in Recorders |-> {}], decided |-> Nil]]
    /\ msgs = UNION {RequestsOf(p, pst[p]) : p \in Proposers}

\* Recorder.record followed by its summary (Alg. 3).
Record(m) ==
    LET g == reg[m.to]
        new == IF m.step = g.step
               THEN IF g.agg # Nil /\ ~Gt(m.prop, g.agg) THEN g ELSE [g EXCEPT !.agg = m.prop]
               ELSE IF m.step > g.step
                    THEN [step |-> m.step, first |-> m.prop, agg |-> m.prop,
                          prev |-> IF g.step + 1 = m.step THEN g.agg ELSE Nil]
                    ELSE g
    IN /\ m \in msgs /\ m.type = "req"
       /\ reg' = [reg EXCEPT ![m.to] = new]
       /\ msgs' = msgs \cup {[type |-> "rep", from |-> m.to, to |-> m.from, req |-> m.step,
                             step |-> new.step, first |-> new.first, prev |-> new.prev]}
       /\ UNCHANGED pst

\* Moves proposer state `s` to `step` with working proposal `cur`, drawing new requests.
Advance(p, s, step, cur) ==
    IF step > MaxStep THEN {s}
    ELSE {[s EXCEPT !.step = step, !.p = cur, !.sent = d,
                    !.replies = [r \in Recorders |-> {}]] : d \in Draws(p, step, cur)}

\* SlotProposer.on_reply (Alg. 4).
OnReply(m) ==
    LET p == m.to
        s == pst[p]
        got == [s.replies EXCEPT ![m.from] = {m}]
        reps == UNION {got[r] : r \in Recorders}
        firsts == {x.first : x \in reps}
        prevs == {x.prev : x \in reps} \ {Nil}
        best == IF prevs = {} THEN Nil ELSE Max(prevs)
        next(cur) == Advance(p, s, s.step + 1, cur)
    IN /\ m \in msgs /\ m.type = "rep"
       /\ UNCHANGED reg
       /\ IF s.decided # Nil \/ m.req # s.step THEN UNCHANGED pst
          ELSE IF m.step > s.step
          THEN IF m.first = Nil THEN UNCHANGED pst
               ELSE \E t \in Advance(p, s, m.step, m.first) : pst' = [pst EXCEPT ![p] = t]
          ELSE IF m.step < s.step THEN UNCHANGED pst
          ELSE IF Cardinality({r \in Recorders : got[r] # {}}) < Quorum
          THEN pst' = [pst EXCEPT ![p].replies = got]
          ELSE CASE s.step % 4 = 0 ->
                      IF s.step = FirstStep /\ \A f \in firsts : f # Nil /\ f.prio = H
                      THEN pst' = [pst EXCEPT ![p].replies = got,
                                              ![p].decided = CHOOSE f \in firsts : TRUE]
                      ELSE \E t \in next(IF firsts \ {Nil} = {} THEN s.p ELSE Max(firsts \ {Nil})) :
                             pst' = [pst EXCEPT ![p] = t]
                 [] s.step % 4 = 2 ->
                      IF best = s.p
                      THEN pst' = [pst EXCEPT ![p].replies = got, ![p].decided = s.p]
                      ELSE \E t \in next(s.p) : pst' = [pst EXCEPT ![p] = t]
                 [] s.step % 4 = 3 ->
                      \E t \in next(IF best = Nil THEN s.p ELSE best) : pst' = [pst EXCEPT ![p] = t]
                 [] OTHER -> \E t \in next(s.p) : pst' = [pst EXCEPT ![p] = t]

\* A proposer that enters a new step sends its requests for it.
Deliver(m) ==
    /\ OnReply(m)
    /\ msgs' = (msgs \ {m}) \cup (IF pst'[m.to].step # pst[m.to].step THEN RequestsOf(m.to, pst'[m.to]) ELSE {})

Next == \E m \in msgs : Record(m) \/ Deliver(m)

Spec == Init /\ [][Next]_vars

Symmetry == Permutations(Recorders)

-----------------------------------------------------------------------------
Decided == {pst[p].decided : p \in Proposers} \ {Nil}

\* G1: every proposer that decides, decides the same value.
Agreement == \A a, b \in Decided : a.key = b.key

\* Only proposed values are decided (keys name the proposers' values).
Validity == \A d \in Decided : d.key \in Proposers

TypeOK ==
    /\ \A r \in Recorders : reg[r].step \in 0..MaxStep
    /\ \A p \in Proposers : pst[p].step \in FirstStep..MaxStep
=============================================================================

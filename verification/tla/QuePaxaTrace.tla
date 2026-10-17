--------------------------- MODULE QuePaxaTrace ---------------------------
(***************************************************************************)
(* Trace validation: does the Rust implementation only do what QuePaxa.tla *)
(* allows? crates/quepaxa/src/slot_sim.rs drives the real Register and     *)
(* SlotProposer over a faulty network and logs every message it delivers  *)
(* with the abstract state after it (module TraceData: every register,    *)
(* and every proposer's step, working value, requests and decision).      *)
(*                                                                         *)
(* Each logged delivery must be the spec's action for that message, which *)
(* must be in the spec's network (a request the spec lets the proposer    *)
(* send, a reply a spec recorder produced), and must lead to the logged   *)
(* state. One exception: the spec delivers a reply at most once, while    *)
(* the implementation may deliver a duplicate: a reply the spec already   *)
(* delivered may arrive again, and must change nothing.                   *)
(* Random priorities are the logged ones, checked against the spec's draw *)
(* rule (MayDraw) instead of enumerating every possible draw.             *)
(*                                                                         *)
(* The invariant Unfinished is violated exactly when the whole trace is   *)
(* matched, which accepts it; otherwise TLC stops at the first delivery   *)
(* the spec does not allow.                                               *)
(***************************************************************************)
EXTENDS QuePaxa, TraceData, Sequences

VARIABLES i,         \* index of the last matched trace entry
          delivered  \* replies the spec has delivered (and so consumed)

View == [reg |-> reg,
         pst |-> [p \in Proposers |-> [step |-> pst[p].step, p |-> pst[p].p,
                                       sent |-> pst[p].sent, decided |-> pst[p].decided]]]

\* Replaces DrawPool (see QuePaxaTrace.cfg): the draw logged next. Draws still keeps it
\* only if the spec's rule (MayDraw) allows it.
TracePool(p, step, cur) == {Trace[i + 1].state.pst[p].sent}

\* Init, with the logged first draws.
TraceInit ==
    /\ i = 1
    /\ reg = [r \in Recorders |-> [step |-> 0, first |-> Nil, agg |-> Nil, prev |-> Nil]]
    /\ pst = [p \in Proposers |-> [step |-> FirstStep, p |-> [prio |-> H, key |-> p],
                sent |-> Trace[1].state.pst[p].sent, replies |-> [r \in Recorders |-> {}],
                decided |-> Nil]]
    /\ \A p \in Proposers : MayDraw(p, FirstStep, [prio |-> H, key |-> p], pst[p].sent)
    /\ msgs = UNION {RequestsOf(p, pst[p]) : p \in Proposers}
    /\ delivered = {}
    /\ View = Trace[1].state

TraceNext ==
    /\ i < Len(Trace)
    /\ i' = i + 1
    /\ LET e == Trace[i + 1].event
       IN IF e.kind = "req"
          THEN /\ Record([type |-> "req", from |-> e.from, to |-> e.to, step |-> e.step, prop |-> e.prop])
               /\ UNCHANGED delivered
          ELSE LET m == [type |-> "rep", from |-> e.from, to |-> e.to, req |-> e.req,
                         step |-> e.step, first |-> e.first, prev |-> e.prev]
               IN IF m \in msgs
                  THEN Deliver(m) /\ delivered' = delivered \cup {m}
                  ELSE m \in delivered /\ UNCHANGED <<vars, delivered>>
    /\ View' = Trace[i + 1].state

\* Violated once the whole trace is matched.
Unfinished == i < Len(Trace)
=============================================================================

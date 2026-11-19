----------------------------- MODULE Entries ------------------------------
(***************************************************************************)
(* Where entries wait until they commit, as implemented in                *)
(* crates/quepaxa/src/replica/entries.rs and entries/back.rs and driven   *)
(* by replica/proposing.rs and replica/watch.rs (#187), written apart     *)
(* from the Rust code. One replica, Origin, submits the entries; at every *)
(* other replica they are another member's.                               *)
(*                                                                         *)
(* Per replica: pending (to batch when proposing directly, else to hand   *)
(* to the target), relayed (proposed here whatever this replica leads),   *)
(* and local batches, each in at most one slot. The origin keeps a        *)
(* forwarded record of what it handed on, which its hedges (Late) and     *)
(* suspicions (Suspect) take back. Forwards travel in a lossy network; a  *)
(* refused forward (the queue cap) is a lost one.                         *)
(*                                                                         *)
(* A slot decides one of the batches proposed in it, or a value from     *)
(* outside the model (DecideOther); the decision is applied and learned   *)
(* everywhere at once, and every other batch in the slot loses it:        *)
(* - a replica proposing directly keeps the batch for another slot;       *)
(* - otherwise its entries not yet committed are handed back (#185): own  *)
(*   ones to pending, other members' to relayed, or to pending when the   *)
(*   slot went to the target deciding (deciding); none of the other       *)
(*   members' when the batch was handed on by stepping aside (#224, D2).  *)
(*                                                                         *)
(* Copies: a stepped-aside leader keeps its batches in their slots while  *)
(* their entries go on; a batch that lost its slot hands own entries on   *)
(* again although a copy may be on its way (D2, own half); a deputy may   *)
(* hand own entries back to their origin (D3); hedges propose what the    *)
(* target also holds. All are allowed here exactly as in the code; dedup  *)
(* (the set of committed entries) absorbs the second copy.                *)
(*                                                                         *)
(* Checked: AtMostOnce (an entry is applied at most once), NoSilentLoss   *)
(* (an entry not yet committed is held somewhere, the forwarded record    *)
(* included, or in flight), HeldDownstream (what the origin handed on is  *)
(* held past its forwarded record or in flight, unless a forward carrying *)
(* it was lost: the record is the trigger to hedge, not the only copy;    *)
(* the origin's hedges would otherwise mask any replica dropping another  *)
(* member's entry), and AtLeastOnce (every submitted entry commits) under *)
(* weak fairness of delivery, driving, deciding, Late, Suspect and the    *)
(* disk catching up, with at most MaxLoss contended or outside-won slots, *)
(* MaxDrop lost forwards, one leader handover and MaxAside steps aside    *)
(* (with forwards lost forever, Suspect and Hear could keep the origin's  *)
(* record from ever staying put long enough to hedge).                    *)
(*                                                                         *)
(* Not modelled: timers (Late and Suspect may fire at any time), batch    *)
(* sizes and the queue cap (a refusal is a loss), the window's slot       *)
(* numbers and order (slots are reused once decided), the leader order    *)
(* seen per replica (one order changes for all at once, so no follower    *)
(* lags), snapshots, fencing, crashes. run.sh checks that seeded bugs are *)
(* found.                                                                 *)
(***************************************************************************)
EXTENDS Integers, FiniteSets, Sequences

CONSTANTS Replicas,   \* e.g. {1, 2, 3}
          Origin,     \* the replica that submits the entries
          E,          \* the entries
          Order1,     \* the leader order (first: the leader), as a sequence
          Order2,     \* the order after the one handover, another leader first
          NS,         \* slots 0..NS-1 in flight at once
          W,          \* the window: slots a replica proposes in at once
          MaxLoss,    \* contended starts plus outside-won slots
          MaxAside,   \* times a leader steps aside
          MaxDrop     \* forwards lost or refused

\* Leader orders for the configurations (a .cfg file cannot write a sequence).
O123 == <<1, 2, 3>>
O231 == <<2, 3, 1>>

Slots == 0..NS-1
NoSlot == -1
Batch == [ents : SUBSET E, slot : Slots \cup {NoSlot}, handed : BOOLEAN]
Msg == [to : Replicas, ents : SUBSET E]

VARIABLES submitted,  \* entries the origin submitted
          pending,    \* pending[r]
          relayed,    \* relayed[r]
          fwd,        \* the origin's forwarded record
          batches,    \* batches[r]: a set of Batch
          msgs,       \* forwards in flight
          susp,       \* members the origin suspects
          aside,      \* aside[r]: r stepped aside
          order,      \* the leader order
          dedup,      \* committed entries
          count,      \* count[e]: times e was applied
          lost,       \* entries some lost forward carried (history)
          left        \* faults left: [loss, aside, handover, drop]

vars == <<submitted, pending, relayed, fwd, batches, msgs, susp, aside, order,
          dedup, count, lost, left>>

Pos(q, n) == CHOOSE i \in 1..Len(q) : q[i] = n
Susp(r) == IF r = Origin THEN susp ELSE {}

\* watch.rs: the first member of the order r does not suspect (r itself when it comes
\* first); the first unsuspected member after r (r if none); the target r hands its
\* entries to.
FirstUnsusp(r) ==
    order[CHOOSE i \in 1..Len(order) :
             /\ (order[i] = r \/ order[i] \notin Susp(r))
             /\ \A j \in 1..i-1 : order[j] # r /\ order[j] \in Susp(r)]
NextAfter(r) ==
    LET later == {i \in Pos(order, r)+1..Len(order) : order[i] \notin Susp(r)}
    IN IF later = {} THEN r ELSE order[CHOOSE i \in later : \A j \in later : i <= j]
Target(r) == IF FirstUnsusp(r) = r /\ aside[r] THEN NextAfter(r) ELSE FirstUnsusp(r)
Direct(r) == Target(r) = r
Leads(r) == order[1] = r

InSlot(r, s) == {b \in batches[r] : b.slot = s}
Slotted(r) == {b \in batches[r] : b.slot # NoSlot}
\* Other members' entries here (the deputy); every entry is another member's but at Origin.
Deputy(r) == r # Origin /\ (relayed[r] # {} \/ \E b \in batches[r] : b.ents # {})
\* proposing.rs `drive`: a follower hedging around a target that talks takes one slot.
Cap(r) == IF Direct(r) \/ Deputy(r) THEN W ELSE 1

Held(e) ==
    \E r \in Replicas : e \in pending[r] \cup relayed[r] \/ \E b \in batches[r] : e \in b.ents
InFlight(e) == \E m \in msgs : e \in m.ents

TypeOK ==
    /\ submitted \subseteq E /\ fwd \subseteq E /\ dedup \subseteq E /\ lost \subseteq E
    /\ pending \in [Replicas -> SUBSET E] /\ relayed \in [Replicas -> SUBSET E]
    /\ batches \in [Replicas -> SUBSET Batch]
    /\ \A r \in Replicas, s \in Slots : Cardinality(InSlot(r, s)) <= 1
    /\ msgs \subseteq Msg /\ susp \subseteq Replicas \ {Origin}
    /\ aside \in [Replicas -> BOOLEAN] /\ count \in [E -> 0..2]

Init ==
    /\ submitted = {} /\ fwd = {} /\ dedup = {} /\ lost = {} /\ msgs = {} /\ susp = {}
    /\ pending = [r \in Replicas |-> {}] /\ relayed = [r \in Replicas |-> {}]
    /\ batches = [r \in Replicas |-> {}] /\ aside = [r \in Replicas |-> FALSE]
    /\ order = Order1 /\ count = [e \in E |-> 0]
    /\ left = [loss |-> MaxLoss, aside |-> MaxAside, handover |-> 1, drop |-> MaxDrop]

\* `submit`.
Submit(e) ==
    /\ e \notin submitted
    /\ submitted' = submitted \cup {e}
    /\ pending' = [pending EXCEPT ![Origin] = @ \cup {e}]
    /\ UNCHANGED <<relayed, fwd, batches, msgs, susp, aside, order, dedup, count, lost,
                   left>>

\* `forward_pending` (`route`): everything pending goes to the target; the origin records
\* its own entries as forwarded.
Forward(r) ==
    /\ ~Direct(r) /\ pending[r] # {}
    /\ msgs' = msgs \cup {[to |-> Target(r), ents |-> pending[r]]}
    /\ fwd' = IF r = Origin THEN fwd \cup pending[r] ELSE fwd
    /\ pending' = [pending EXCEPT ![r] = {}]
    /\ UNCHANGED <<submitted, relayed, batches, susp, aside, order, dedup, count, lost,
                   left>>

\* `on_forward` (`received`): to pending as the leader or stepped aside (to hand on),
\* else to relayed as a deputy.
Receive(m) ==
    LET r == m.to IN
    /\ m \in msgs
    /\ msgs' = msgs \ {m}
    /\ IF Leads(r) \/ aside[r]
         THEN pending' = [pending EXCEPT ![r] = @ \cup m.ents] /\ UNCHANGED relayed
         ELSE relayed' = [relayed EXCEPT ![r] = @ \cup m.ents] /\ UNCHANGED pending
    /\ UNCHANGED <<submitted, fwd, batches, susp, aside, order, dedup, count, lost,
                   left>>
Deliver(m) == m.to # Origin /\ Receive(m)
\* D3: a deputy (or a leader stepping aside, or losing the lead) hands the origin's own
\* entries back to it; they land as copies beside its forwarded record.
DeputyHandBack(m) == m.to = Origin /\ Receive(m)

\* A forward lost on the way, or refused.
Drop(m) ==
    /\ m \in msgs /\ left.drop > 0
    /\ msgs' = msgs \ {m}
    /\ lost' = lost \cup m.ents
    /\ left' = [left EXCEPT !.drop = @ - 1]
    /\ UNCHANGED <<submitted, pending, relayed, fwd, batches, susp, aside, order, dedup,
                   count>>

\* `drive`: prune batches with nothing left to commit; propose a waiting batch in slot s,
\* or cut one from relayed (and pending when proposing directly) and propose it there
\* (`cut`, `start`). Joining a slot another replica proposes in contends for it.
Drive(r, s) ==
    LET live == {b \in batches[r] : b.slot # NoSlot \/ b.ents \ dedup # {}}
        waiting == {b \in live : b.slot = NoSlot}
        take == relayed[r] \cup (IF Direct(r) THEN pending[r] ELSE {})
        contended == \E q \in Replicas \ {r} : InSlot(q, s) # {}
        starts == waiting # {} \/ take \ dedup # {}
    IN /\ Cardinality({b \in live : b.slot # NoSlot}) < Cap(r)
       /\ InSlot(r, s) = {}
       /\ waiting # {} \/ take # {}
       /\ starts /\ contended => left.loss > 0
       /\ left' = IF starts /\ contended THEN [left EXCEPT !.loss = @ - 1] ELSE left
       /\ IF waiting # {}
            THEN /\ \E b \in waiting : batches' = [batches EXCEPT ![r] = (live \ {b}) \cup {[b EXCEPT !.slot = s]}]
                 /\ UNCHANGED <<pending, relayed>>
            ELSE /\ batches' = [batches EXCEPT ![r] = live \cup
                       (IF take \ dedup = {} THEN {} ELSE {[ents |-> take \ dedup, slot |-> s, handed |-> FALSE]})]
                 /\ relayed' = [relayed EXCEPT ![r] = {}]
                 /\ pending' = [pending EXCEPT ![r] = IF Direct(r) THEN {} ELSE @]
       /\ UNCHANGED <<submitted, fwd, msgs, susp, aside, order, dedup, count, lost>>

\* What a batch that lost its slot hands back at r, not proposing directly (`lost_slot`):
\* entries not committed, own ones to pending, other members' to relayed or (deciding) to
\* pending, none of theirs if the batch was handed on.
Keep(b) == b.ents \ dedup
Others(r, b) == IF r = Origin THEN {} ELSE IF b.handed THEN {} ELSE Keep(b)
Mine(r, b) == IF r = Origin THEN Keep(b) ELSE {}

\* Slot s is decided with entries won (proposed by w); dec[r]: the slot went to r's target
\* deciding. Learned everywhere at once: applied (dedup), own entries resolved at the
\* origin, every other batch in s lost (`learn`, `lost_slot`, `lost_slot_kept`).
Settle(s, w, won, isWin(_, _), dec) ==
    /\ \A r \in Replicas : dec[r] => w = Target(r)
    /\ dedup' = dedup \cup won
    /\ count' = [e \in E |-> IF e \in won \ dedup THEN count[e] + 1 ELSE count[e]]
    /\ fwd' = fwd \ won
    /\ LET lose(r) == {b \in InSlot(r, s) : ~isWin(r, b)}
           back(r) == IF Direct(r) THEN {} ELSE lose(r)
           kept(r) == IF Direct(r) THEN {[b EXCEPT !.slot = NoSlot] : b \in lose(r)} ELSE {}
       IN /\ batches' = [r \in Replicas |-> (batches[r] \ InSlot(r, s)) \cup kept(r)]
          /\ pending' = [r \in Replicas |-> pending[r] \cup
                 UNION {Mine(r, b) \cup (IF dec[r] THEN Others(r, b) ELSE {}) : b \in back(r)}]
          /\ relayed' = [r \in Replicas |-> relayed[r] \cup
                 UNION {IF dec[r] THEN {} ELSE Others(r, b) : b \in back(r)}]
    /\ UNCHANGED <<submitted, msgs, susp, aside, order, lost>>

\* One of the batches in s wins; a replica stepped aside cannot decide its own: its disk
\* is stuck.
DecideBatch(s, w, dec) ==
    /\ ~aside[w]
    /\ \E v \in InSlot(w, s) :
         Settle(s, w, v.ents, LAMBDA r, b : r = w /\ b = v, dec)
    /\ UNCHANGED left
DecideOther(s, w, dec) ==
    /\ left.loss > 0 /\ \E r \in Replicas : InSlot(r, s) # {}
    /\ left' = [left EXCEPT !.loss = @ - 1]
    /\ Settle(s, w, {}, LAMBDA r, b : FALSE, dec)

\* `hand_on_unstarted` (`lost_lead`) at the leader the handover replaces: batches not in a
\* slot go to pending.
Handover ==
    LET old == order[1]
        unstarted == {b \in batches[old] : b.slot = NoSlot}
    IN /\ left.handover > 0
       /\ left' = [left EXCEPT !.handover = @ - 1]
       /\ order' = Order2
       /\ batches' = [batches EXCEPT ![old] = @ \ unstarted]
       /\ pending' = [pending EXCEPT ![old] = @ \cup UNION {Keep(b) : b \in unstarted}]
       /\ aside' = [aside EXCEPT ![old] = FALSE]
       /\ UNCHANGED <<submitted, relayed, fwd, msgs, susp, dedup, count, lost>>

\* `check_aside` (`step_aside`): the replica others hand their entries to, its disk stuck,
\* hands everything on: every batch's entries and everything relayed to pending; batches
\* in a slot stay there, handed.
StepAside(r) ==
    /\ left.aside > 0 /\ ~aside[r]
    /\ FirstUnsusp(r) = r /\ NextAfter(r) # r
    /\ left' = [left EXCEPT !.aside = @ - 1]
    /\ aside' = [aside EXCEPT ![r] = TRUE]
    /\ pending' = [pending EXCEPT ![r] = @ \cup relayed[r] \cup UNION {Keep(b) : b \in batches[r]}]
    /\ relayed' = [relayed EXCEPT ![r] = {}]
    /\ batches' = [batches EXCEPT ![r] = {[b EXCEPT !.handed = TRUE] : b \in Slotted(r)}]
    /\ UNCHANGED <<submitted, fwd, msgs, susp, order, dedup, count, lost>>

\* The disk caught up.
Back(r) ==
    /\ aside[r]
    /\ aside' = [aside EXCEPT ![r] = FALSE]
    /\ UNCHANGED <<submitted, pending, relayed, fwd, batches, msgs, susp, order, dedup,
                   count, lost, left>>

\* `late`: the origin hedges a forwarded entry, proposing it itself, or stepped aside,
\* handing it to the target again.
Late(e) ==
    /\ e \in fwd
    /\ fwd' = fwd \ {e}
    /\ IF aside[Origin]
         THEN pending' = [pending EXCEPT ![Origin] = @ \cup {e}] /\ UNCHANGED relayed
         ELSE relayed' = [relayed EXCEPT ![Origin] = @ \cup {e}] /\ UNCHANGED pending
    /\ UNCHANGED <<submitted, batches, msgs, susp, aside, order, dedup, count, lost,
                   left>>

\* `suspect_target`: the target stayed silent; everything forwarded goes back to pending.
Suspect ==
    /\ fwd # {}
    /\ susp' = IF Target(Origin) # Origin THEN susp \cup {Target(Origin)} ELSE susp
    /\ pending' = [pending EXCEPT ![Origin] = @ \cup fwd]
    /\ fwd' = {}
    /\ UNCHANGED <<submitted, relayed, batches, msgs, aside, order, dedup, count, lost,
                   left>>

\* `heard_from`: a suspected member spoke.
Hear(n) ==
    /\ n \in susp
    /\ susp' = susp \ {n}
    /\ UNCHANGED <<submitted, pending, relayed, fwd, batches, msgs, aside, order, dedup,
                   count, lost, left>>

Decisions == [Replicas -> BOOLEAN]

Next ==
    \/ \E e \in E : Submit(e) \/ Late(e)
    \/ \E r \in Replicas : Forward(r) \/ StepAside(r) \/ Back(r) \/ \E s \in Slots : Drive(r, s)
    \/ \E m \in msgs : Deliver(m) \/ DeputyHandBack(m) \/ Drop(m)
    \/ \E s \in Slots, w \in Replicas, dec \in Decisions : DecideBatch(s, w, dec) \/ DecideOther(s, w, dec)
    \/ Handover \/ Suspect \/ \E n \in Replicas : Hear(n)

Fairness ==
    /\ \A m \in Msg : WF_vars(Deliver(m)) /\ WF_vars(DeputyHandBack(m))
    /\ \A r \in Replicas : WF_vars(Forward(r)) /\ WF_vars(\E s \in Slots : Drive(r, s)) /\ WF_vars(Back(r))
    /\ \A s \in Slots : WF_vars(\E w \in Replicas, dec \in Decisions : DecideBatch(s, w, dec))
    /\ \A e \in E : WF_vars(Late(e))
    /\ WF_vars(Suspect)

Spec == Init /\ [][Next]_vars /\ Fairness

AtMostOnce == \A e \in E : count[e] <= 1
NoSilentLoss == \A e \in submitted \ dedup : Held(e) \/ e \in fwd \/ InFlight(e)
HeldDownstream == \A e \in fwd \ dedup : Held(e) \/ InFlight(e) \/ e \in lost
AtLeastOnce == \A e \in E : e \in submitted ~> e \in dedup
=============================================================================

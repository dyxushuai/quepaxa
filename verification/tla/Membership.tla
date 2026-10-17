---------------------------- MODULE Membership ----------------------------
(***************************************************************************)
(* Membership changes across slots, as implemented in                     *)
(* crates/quepaxa/src/schedule.rs and replica/retire.rs, written apart     *)
(* from the Rust code. QuePaxa.tla checks what happens inside one slot;   *)
(* here a slot is decided atomically by a majority of the members its     *)
(* proposer computes for it, and the model checks the rules around that:  *)
(*                                                                         *)
(* - A change decided in slot t installs new members from slot t + LAG,   *)
(*   unless an earlier change has not taken effect yet (then it is        *)
(*   ignored). A replica drives only slots below its commit index + LAG,  *)
(*   computing their members from the log prefix it knows.                *)
(*   ViewAgree: every replica that may drive a slot computes its true     *)
(*   members, so all its proposers use the same recorders.                *)
(* - Decisions are known in memory first and made durable later; a crash *)
(*   loses what is not durable. Recorders' registers are durable          *)
(*   (persist-before-send), so a live majority of a slot's members can    *)
(*   always recover its decision.                                         *)
(* - A replica removed by a change retires (shuts down for good) once a   *)
(*   majority of the new members durably know every slot before the       *)
(*   epoch, and it has caught up with what they know.                     *)
(*   Recoverable: every decided slot is durably known by a live replica,  *)
(*   or recoverable from a live majority of its members.                  *)
(*                                                                         *)
(* Not modelled: what happens inside a slot (QuePaxa.tla), leader         *)
(* rotation, snapshots, liveness. run.sh checks that seeded bugs in each  *)
(* rule are still found.                                                  *)
(***************************************************************************)
EXTENDS Integers, FiniteSets

CONSTANTS Replicas,     \* e.g. 1..4
          InitMembers,  \* the configured members, e.g. {1, 2, 3}
          Changes,      \* member sets a change may install
          N,            \* slots 0..N-1
          LAG           \* slots between deciding a change and its effect

Slots == 0..N-1
\* A slot's value: nothing decided yet, a write, or a change to members m.
None == [k |-> "none", m |-> {}]
Write == [k |-> "write", m |-> {}]
Change(S) == [k |-> "change", m |-> S]
Values == {Write} \cup {Change(S) : S \in Changes}

VARIABLES decided,  \* decided[s]: the value of slot s, or None
          log,      \* log[r]: slots r knows decided (memory)
          dlog,     \* dlog[r]: slots r knows decided durably
          retired   \* replicas shut down for good

vars == <<decided, log, dlog, retired>>

Maj(S) == {Q \in SUBSET S : 2 * Cardinality(Q) > Cardinality(S)}

\* Length of the contiguous prefix of slots in L (the commit index).
Prefix(L) == CHOOSE n \in 0..N : (\A t \in 0..n-1 : t \in L) /\ (n = N \/ n \notin L)

\* The schedule after folding slots t..last, where val(t) is what a slot holds as far as
\* the folding replica knows (Write if it does not know it).
RECURSIVE Fold(_, _, _, _)
Fold(t, last, st, val) ==
    IF t > last THEN st
    ELSE LET act == IF st.from <= t THEN [st EXCEPT !.cur = st.nxt] ELSE st
             v == val[t]
             next == IF v.k = "change" /\ act.from <= t
                     THEN [act EXCEPT !.nxt = v.m, !.from = t + LAG]
                     ELSE act
         IN Fold(t + 1, last, next, val)

Start == [cur |-> InitMembers, nxt |-> InitMembers, from |-> 0]

\* What replica r knows of every slot, and the truth.
Known(r) == [t \in Slots |-> IF t \in log[r] THEN decided[t] ELSE Write]
Truth == [t \in Slots |-> IF decided[t] = None THEN Write ELSE decided[t]]

\* Members of slot s from every slot before it, as given by val. Only the window rule
\* makes a replica's partial view agree with the truth.
MembersAt(s, val) ==
    LET st == Fold(0, s - 1, Start, val) IN IF st.from <= s THEN st.nxt ELSE st.cur

\* The latest epoch r knows of: its members and first slot.
Latest(r) == Fold(0, Prefix(log[r]) - 1, Start, Known(r))

CanDrive(r, s) ==
    /\ r \notin retired
    /\ s \notin log[r]
    /\ s < Prefix(log[r]) + LAG

Init ==
    /\ decided = [s \in Slots |-> None]
    /\ log = [r \in Replicas |-> {}]
    /\ dlog = [r \in Replicas |-> {}]
    /\ retired = {}

\* r drives s to v with a live majority of the members it computes for s.
Decide(r, s, v) ==
    LET M == MembersAt(s, Known(r))
    IN /\ CanDrive(r, s)
       /\ decided[s] = None
       /\ r \in M
       /\ \E Q \in Maj(M) : Q \cap retired = {}
       /\ decided' = [decided EXCEPT ![s] = v]
       /\ log' = [log EXCEPT ![r] = @ \cup {s}]
       /\ UNCHANGED <<dlog, retired>>

\* r learns s from a live replica that knows it, or by recovering it from a live majority
\* of its members' durable registers.
Learn(r, s) ==
    /\ r \notin retired
    /\ decided[s] # None
    /\ s \notin log[r]
    /\ \/ \E o \in Replicas \ retired : s \in log[o]
       \/ \E Q \in Maj(MembersAt(s, Truth)) : Q \cap retired = {}
    /\ log' = [log EXCEPT ![r] = @ \cup {s}]
    /\ UNCHANGED <<decided, dlog, retired>>

Persist(r) ==
    /\ r \notin retired
    /\ dlog[r] # log[r]
    /\ dlog' = [dlog EXCEPT ![r] = log[r]]
    /\ UNCHANGED <<decided, log, retired>>

Crash(r) ==
    /\ r \notin retired
    /\ log[r] # dlog[r]
    /\ log' = [log EXCEPT ![r] = dlog[r]]
    /\ UNCHANGED <<decided, dlog, retired>>

\* Action::Retired: r is not a member of the latest epoch it knows, a majority of that
\* epoch's members durably know every slot before it, and none knows more than r.
Retire(r) ==
    LET st == Latest(r)
        caught == {m \in st.nxt : Prefix(dlog[m]) >= st.from}
    IN /\ r \notin retired
       /\ st.from > 0
       /\ r \notin st.nxt
       /\ 2 * Cardinality(caught) > Cardinality(st.nxt)
       /\ \A m \in st.nxt : Prefix(dlog[m]) <= Prefix(log[r])
       /\ retired' = retired \cup {r}
       /\ UNCHANGED <<decided, log, dlog>>

Next ==
    \/ \E r \in Replicas, s \in Slots, v \in Values : Decide(r, s, v)
    \/ \E r \in Replicas, s \in Slots : Learn(r, s)
    \/ \E r \in Replicas : Persist(r) \/ Crash(r) \/ Retire(r)

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------
\* Every replica that may drive a slot computes its true members.
ViewAgree ==
    \A r \in Replicas, s \in Slots :
        CanDrive(r, s) => MembersAt(s, Known(r)) = MembersAt(s, Truth)

\* No decided slot is lost.
Recoverable ==
    \A s \in Slots : decided[s] # None =>
        \/ \E r \in Replicas \ retired : s \in dlog[r]
        \/ \E Q \in Maj(MembersAt(s, Truth)) : Q \cap retired = {}

TypeOK ==
    /\ \A s \in Slots : decided[s] \in Values \cup {None}
    /\ \A r \in Replicas : dlog[r] \subseteq Slots /\ log[r] \subseteq Slots
=============================================================================

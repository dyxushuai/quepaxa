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
(*   epoch, and its own read barrier committed: it has applied every slot *)
(*   decided before that point.                                           *)
(*   Recoverable: every decided slot is durably known by a live replica,  *)
(*   or recoverable from a live majority of its members, even with a      *)
(*   minority of every member set from the slot on down as well (so one   *)
(*   durable copy on a new member is not enough).                         *)
(*                                                                         *)
(* Retire is a replica shutting down for good after it reports Retired.  *)
(* Assumed: the guide's operator rule, that a replica a change adds back  *)
(* keeps running until it applied that change (it may learn of the change *)
(* only after reporting Retired, so no implementation can rule it out);   *)
(* and, beyond the guide, that a change never names a replica that shut   *)
(* down. The guide also lets an operator restart a replica a change adds  *)
(* back; that is not covered.                                             *)
(*                                                                         *)
(* Not modelled: what happens inside a slot (QuePaxa.tla), leader         *)
(* rotation, snapshots, liveness. run.sh checks that seeded bugs in each  *)
(* rule are found; MembershipProof.tla proves the rules.                  *)
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

Start == [cur |-> InitMembers, nxt |-> InitMembers, from |-> 0]

\* Slot t folded into the schedule st, where val[t] is what the slot holds as far as the
\* folding replica knows (Write if it does not know it).
FoldStep(st, t, val) ==
    LET act == IF st.from <= t THEN [st EXCEPT !.cur = st.nxt] ELSE st
        v == val[t]
    IN IF v.k = "change" /\ act.from <= t
       THEN [act EXCEPT !.nxt = v.m, !.from = t + LAG]
       ELSE act

\* The schedule after folding slots 0..n-1 of val (a recursive function, which TLAPS can
\* reason about, unlike a recursive operator).
Folded(val) == LET f[n \in Nat] == IF n = 0 THEN Start ELSE FoldStep(f[n - 1], n - 1, val) IN f

\* What replica r knows of every slot, and the truth.
Known(r) == [t \in Slots |-> IF t \in log[r] THEN decided[t] ELSE Write]
Truth == [t \in Slots |-> IF decided[t] = None THEN Write ELSE decided[t]]

\* Members of slot s from every slot before it, as given by val. Only the window rule
\* makes a replica's partial view agree with the truth.
MembersAt(s, val) ==
    LET st == Folded(val)[s] IN IF st.from <= s THEN st.nxt ELSE st.cur

\* The latest epoch r knows of: its members and first slot.
Latest(r) == Folded(Known(r))[Prefix(log[r])]

CanDrive(r, s) ==
    /\ r \notin retired
    /\ s \notin log[r]
    /\ s < Prefix(log[r]) + LAG

Init ==
    /\ decided = [s \in Slots |-> None]
    /\ log = [r \in Replicas |-> {}]
    /\ dlog = [r \in Replicas |-> {}]
    /\ retired = {}

\* r drives s to v with a live majority of the members it computes for s. A change never
\* names a retired replica: the operator does not add back a replica that shut down.
Decide(r, s, v) ==
    LET M == MembersAt(s, Known(r))
    IN /\ CanDrive(r, s)
       /\ decided[s] = None
       /\ r \in M
       /\ \E Q \in Maj(M) : Q \cap retired = {}
       /\ v.m \cap retired = {}
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

\* r shuts down for good after Action::Retired: r is not a member of the latest epoch it
\* knows (from the prefix it applied, as its read barrier commits in order), and a majority
\* of that epoch's members durably know every slot before it. Later slots may be decided
\* and unknown to r. The operator rule: a change that adds r back keeps it running, so r
\* does not shut down while a decided change it has not applied names it (it may still
\* have reported Retired).
Retire(r) ==
    LET st == Latest(r)
        caught == {m \in st.nxt : Prefix(dlog[m]) >= st.from}
    IN /\ r \notin retired
       /\ st.from > 0
       /\ r \notin st.nxt
       /\ 2 * Cardinality(caught) > Cardinality(st.nxt)
       /\ \A t \in Slots : decided[t] # None /\ Truth[t].k = "change" /\ r \in Truth[t].m =>
              t < Prefix(log[r])
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

\* The members in force after the last slot, and those of a change still pending.
Current == Folded(Truth)[N]

\* The member sets from slot s on, a pending one included: the ones that may hold slot s,
\* its own members by their registers or, after some of them retired, a later epoch's
\* majority by durable copies.
LaterEpochs(s) == {MembersAt(t, Truth) : t \in s..N} \cup {Current.cur, Current.nxt}

\* Replicas the cluster may lose on top of the retired ones, as far as slot s is concerned:
\* a minority of each of those sets. (A minority of the current members alone is not
\* enough: until a majority of a new epoch durably holds the old slots, losing an old member
\* and a new one can lose an old slot.)
Tolerable(s, D) == \A E \in LaterEpochs(s) : 2 * Cardinality(D \cap E) < Cardinality(E)

\* No decided slot is lost, even with a tolerable set of replicas down as well.
Recoverable ==
    \A s \in Slots : decided[s] # None =>
        LET M == MembersAt(s, Truth)
        IN \A D \in SUBSET Replicas :
            Tolerable(s, D) =>
                LET gone == retired \cup D
                IN \/ \E r \in Replicas \ gone : s \in dlog[r]
                   \/ 2 * Cardinality(M \ gone) > Cardinality(M)

TypeOK ==
    /\ \A s \in Slots : decided[s] \in Values \cup {None}
    /\ \A r \in Replicas : dlog[r] \subseteq Slots /\ log[r] \subseteq Slots
=============================================================================

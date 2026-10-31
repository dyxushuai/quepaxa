------------------------------- MODULE Reads -------------------------------
(***************************************************************************)
(* Linearizable reads without a log slot, as implemented in               *)
(* crates/quepaxa/src/replica/reads.rs, written apart from the Rust code. *)
(* It extends Membership.tla: slots are decided atomically, membership    *)
(* changes take effect LAG slots later, replicas crash and retire.        *)
(*                                                                         *)
(* Each recorder holds registers for the slots it recorded: in memory     *)
(* first, durable once persisted. A slot is decided only on replies from  *)
(* a majority of its members, each sent after the register was durable.   *)
(* A register is dropped once the slot's decision is durable at that      *)
(* replica, and decided slots below a floor are compacted away.           *)
(*                                                                         *)
(* A read at replica r asks recorders for their mark, the exclusive bound *)
(* of every slot they hold state for, from memory: registers, decided     *)
(* slots above the floor, the floor itself. It takes K as the largest     *)
(* answer (at least 1, so the first read of a fresh cluster waits for    *)
(* slot 0), and is ready once r has applied every slot below K and, for   *)
(* every member set r knows that covers a slot past what it applied, a    *)
(* majority of it answered after the read was requested.                  *)
(*                                                                         *)
(* ReadSafe: a ready read covers every slot decided before it was         *)
(* requested. That also makes reads monotone: a read ready earlier had    *)
(* every slot below its K decided, so a later read covers them too; one   *)
(* read per configuration suffices. run.sh checks that seeded bugs in     *)
(* each rule are found.                                                    *)
(***************************************************************************)
EXTENDS Membership, TLC

CONSTANTS ReadIds   \* e.g. {1, 2}

VARIABLES vreg,     \* vreg[r]: slots r holds registers for in memory only
          reg,      \* reg[r]: slots r holds durable registers for
          floor,    \* floor[r]: decided slots below it are compacted
          rd        \* rd[i]: read i

rvars == <<vreg, reg, floor, rd>>
allvars == <<decided, log, dlog, retired, vreg, reg, floor, rd>>

Max(S) == CHOOSE m \in S : \A x \in S : x <= m

\* What r answers: from memory, durable or not.
Mark(r) ==
    LET held == vreg[r] \cup reg[r] \cup {t \in log[r] : t >= floor[r]}
    IN Max({floor[r]} \cup {t + 1 : t \in held})

\* What r would answer after a crash.
DurableMark(r) ==
    LET held == reg[r] \cup {t \in dlog[r] : t >= floor[r]}
    IN Max({floor[r]} \cup {t + 1 : t \in held})

\* A read: its state, replica, the slots decided before it was requested, who answered it,
\* the largest answer (at least 1), and the prefix its replica had applied when it was
\* ready (what the read reflects).
Idle == [st |-> "idle", at |-> 0, before |-> {}, who |-> {}, k |-> 1, applied |-> 0]

\* The member sets r knows that cover a slot at or above k: those of slots k..N-1 as r
\* computes them, and the epoch it knows to be in force or pending.
Covering(r, k) ==
    LET st == Latest(r) IN {MembersAt(t, Known(r)) : t \in k..N-1} \cup {st.cur, st.nxt}

RInit ==
    /\ Init
    /\ vreg = [r \in Replicas |-> {}]
    /\ reg = [r \in Replicas |-> {}]
    /\ floor = [r \in Replicas |-> 0]
    /\ rd = [i \in ReadIds |-> Idle]

\* r records slot s in memory (a record request from some proposer).
Record(r, s) ==
    /\ r \notin retired
    /\ r \in MembersAt(s, Truth)
    /\ s >= floor[r]
    /\ decided[s] = None
    /\ s \notin vreg[r] \cup reg[r]
    /\ vreg' = [vreg EXCEPT ![r] = @ \cup {s}]
    /\ UNCHANGED <<decided, log, dlog, retired, reg, floor, rd>>

PersistRegisters(r) ==
    /\ r \notin retired
    /\ vreg[r] # {}
    /\ reg' = [reg EXCEPT ![r] = @ \cup vreg[r]]
    /\ vreg' = [vreg EXCEPT ![r] = {}]
    /\ UNCHANGED <<decided, log, dlog, retired, floor, rd>>

\* Decided on durable registers of a live majority of the members r computes for s.
RDecide(r, s, v) ==
    LET M == MembersAt(s, Known(r))
    IN \E Q \in Maj(M) :
        /\ Q \cap retired = {}
        /\ Decide(r, s, v)
        /\ reg' = [q \in Replicas |-> IF q \in Q THEN reg[q] \cup {s} ELSE reg[q]]
        /\ vreg' = [q \in Replicas |-> IF q \in Q THEN vreg[q] \ {s} ELSE vreg[q]]
        /\ UNCHANGED <<floor, rd>>

\* A register goes once the decision of its slot is durable here.
DropRegister(r, s) ==
    /\ s \in reg[r] \cup vreg[r]
    /\ s \in dlog[r]
    /\ reg' = [reg EXCEPT ![r] = @ \ {s}]
    /\ vreg' = [vreg EXCEPT ![r] = @ \ {s}]
    /\ UNCHANGED <<decided, log, dlog, retired, floor, rd>>

\* Durably known prefix compacted (a checkpoint).
Compact(r) ==
    /\ r \notin retired
    /\ Prefix(dlog[r]) > floor[r]
    /\ floor' = [floor EXCEPT ![r] = Prefix(dlog[r])]
    /\ reg' = [reg EXCEPT ![r] = {t \in @ : t >= Prefix(dlog[r])}]
    /\ vreg' = [vreg EXCEPT ![r] = {t \in @ : t >= Prefix(dlog[r])}]
    /\ UNCHANGED <<decided, log, dlog, retired, rd>>

RCrash(r) ==
    /\ r \notin retired
    /\ log[r] # dlog[r] \/ vreg[r] # {}
    /\ log' = [log EXCEPT ![r] = dlog[r]]
    /\ vreg' = [vreg EXCEPT ![r] = {}]
    /\ UNCHANGED <<decided, dlog, retired, reg, floor, rd>>

Request(i, r) ==
    /\ rd[i].st = "idle"
    /\ r \notin retired
    /\ rd' = [rd EXCEPT ![i] = [st |-> "open", at |-> r,
                                before |-> {t \in Slots : decided[t] # None}, who |-> {}, k |-> 1,
                                applied |-> 0]]
    /\ UNCHANGED <<decided, log, dlog, retired, vreg, reg, floor>>

\* q answers read i's round (sent after the read was requested).
Answer(i, q) ==
    /\ rd[i].st = "open"
    /\ q \notin retired
    /\ q \notin rd[i].who
    /\ rd' = [rd EXCEPT ![i].who = @ \cup {q}, ![i].k = Max({@, Mark(q)})]
    /\ UNCHANGED <<decided, log, dlog, retired, vreg, reg, floor>>

Ready(i) ==
    LET r == rd[i].at
        A == rd[i].who
        K == rd[i].k
    IN /\ rd[i].st = "open"
       /\ r \notin retired
       /\ Prefix(log[r]) >= K
       /\ \A E \in Covering(r, Prefix(log[r])) : 2 * Cardinality(A \cap E) > Cardinality(E)
       /\ rd' = [rd EXCEPT ![i].st = "ready", ![i].applied = Prefix(log[r])]
       /\ UNCHANGED <<decided, log, dlog, retired, vreg, reg, floor>>

RNext ==
    \/ \E r \in Replicas, s \in Slots, v \in Values : RDecide(r, s, v)
    \/ \E r \in Replicas, s \in Slots : (Learn(r, s) /\ UNCHANGED rvars)
                                        \/ Record(r, s) \/ DropRegister(r, s)
    \/ \E r \in Replicas : (Persist(r) /\ UNCHANGED rvars) \/ RCrash(r)
                           \/ (Retire(r) /\ UNCHANGED rvars)
                           \/ PersistRegisters(r) \/ Compact(r)
    \/ \E i \in ReadIds : Ready(i) \/ \E r \in Replicas : Request(i, r) \/ Answer(i, r)

RSpec == RInit /\ [][RNext]_allvars

-----------------------------------------------------------------------------
\* A ready read reflects every slot decided before it was requested.
ReadSafe == \A i \in ReadIds : rd[i].st = "ready" => \A t \in rd[i].before : t < rd[i].applied

\* The durable mark never exceeds what is answered from memory.
MarkOrder == \A r \in Replicas : DurableMark(r) <= Mark(r)

ReplicaSymmetry == Permutations(Replicas)
=============================================================================

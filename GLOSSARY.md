# QuePaxa

An open-source Rust implementation of the QuePaxa crash-fault-tolerant consensus protocol, exposed as a replicated log.

## Language

**Replica**:
One member of a cluster of 2f+1 that stores the log and takes part in consensus.
_Avoid_: Node, server, peer

**Log**:
The totally ordered sequence of slots every replica agrees on; entries are opaque bytes to the protocol.
_Avoid_: Journal, WAL

**Slot**:
One position in the log, decided by one instance of consensus.
_Avoid_: Index, instance, height

**Decided**:
A slot is decided once its value can never change; replicas that decide a slot agree on its value.
_Avoid_: Committed, chosen

**Read barrier**:
A log entry proposed to obtain a slot position after which a linearizable read may be served.
_Avoid_: Read index, sync

**Proposer**:
The active role within a replica that drives consensus on a slot by sending record requests to recorders.
_Avoid_: Coordinator

**Recorder**:
The passive role within a replica that holds one register per slot and answers record requests.
_Avoid_: Acceptor, voter

**Leader**:
The one proposer allowed the reserved top priority in the first round of a slot, giving a one-round-trip decision; optional for progress.
_Avoid_: Primary, master

**Proposal**:
A value paired with a priority and its proposer's identity, ordered as a single comparable quantity.
_Avoid_: Ballot

**Step**:
The logical clock of a slot, combining its round and phase; every step is one round trip to a majority of recorders.
_Avoid_: Term, epoch

**Batch**:
The opaque payload decided in one slot, made of one or more proposed entries.

**Fenced**:
The state of a replica that has lost durability guarantees and stops answering until restarted.

**Hedge delay**:
How long a replica waits for others to make progress before driving work itself; it only affects performance, never safety.
_Avoid_: Timeout, election timeout

**Checkpoint**:
A durable rewrite of a replica's records that replaces everything persisted before it, emitted after compaction.
_Avoid_: Snapshot (that is the application's state)

**Snapshot**:
The application's state up to some slot, stored together with the replica's core state so log entries before it can be discarded.
_Avoid_: Checkpoint

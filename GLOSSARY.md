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

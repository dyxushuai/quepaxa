# What carries over from Raft implementations, and what does not

Raft libraries such as openraft solve problems a QuePaxa engine also meets, but their answers rest on a leader that is the only writer, holds a term, and streams the log to each follower. A QuePaxa leader is only each slot's preferred proposer: any replica may propose in any slot, the leader order is derived from the decided log, and followers learn decisions from `Decided` messages or by pulling with `CatchUp`. So we borrow the problem and redo the mechanism.

- **Learners** become observers that pull decided slots from any member and read through the existing majority round (`ReadQuery`), instead of riding a leader's replication stream and reading through the leader.
- **Leader transfer** is a leader-order change decided in the log and in force `LAG` slots later, through the path membership changes take. It needs no write pause and no catch-up precondition for safety.
- **Separating heartbeats from replication** becomes keeping bulk transfers (snapshots, catch-up) off the link that carries consensus messages. We have no heartbeats or elections; what a delayed `ForwardAck` or `Decided` costs here is a follower wrongly suspecting a live leader and hedging.
- **Metrics** report what QuePaxa has: per-member round trip and committed progress, the leader order and epoch, the fast-path share, and hedges, suspicions and step-asides. There are no terms, votes or per-follower match indexes.

We do not adopt three things:

- **Joint consensus.** A single-step change in force `LAG` slots after it is decided is already safe, and the window rule is proved (`MembershipProof.tla`).
- **Leader-lease reads.** A lease needs a leader with exclusive write authority, which a QuePaxa leader never has, and it would make safety depend on bounded clock drift. A read that skips the majority round needs a different mechanism, such as recorders promising readers, with its own proof.
- **An async runtime inside the library** ([ADR 0001](0001-sans-io-core.md)). Easier integration belongs in a reference runtime or an optional adapter crate.

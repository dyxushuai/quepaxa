# Linearizable reads keep the majority round (no zero-RTT leases)

Issue #158 asked whether `quepaxa` can serve linearizable reads without the majority round
trip of [ADR 0003](0003-quorum-reads.md). Our design evaluation
([`docs/design/reads-without-quorum-rtt.md`](../design/reads-without-quorum-rtt.md)) reaches
a **No-Go** decision for the core engine.

Because QuePaxa has no exclusive leader (any replica may propose and decide any slot), a
zero-RTT linearizable read requires recorders to promise not to accept new slots. Without
clocks, a single crashed or partitioned lease-holding reader halts all cluster writes and
reconfigurations indefinitely (violating FR5 liveness without timeouts). With clocks (Paxos
Quorum Leases), safety (FR2) becomes coupled to physical clock drift and bounded process
pauses — breaking the design matrix invariant that safety never reads a timing parameter —
and a crashed reader still stalls writes for the lease duration. Requiring all `N` members
to acknowledge writes (`Q_W = N, Q_R = 1`) removes write fault tolerance altogether.

`quepaxa` therefore keeps coalesced closest-majority PQR (`Replica::read_barrier`, proved in
`ReadsProof.tla`) for linearizable reads, combined with non-voting regional observers
(`Config::follow_interval`, #157) and zero-RTT local reads (stale or host-tracked
read-your-own-writes) for workloads that do not need global linearizability.

# QuePaxa vs etcd

Compares a three-replica `kv-tokio` cluster (QuePaxa) with a three-member etcd v3.7.2
cluster on one host. Both are driven through etcd's v3 KV gRPC API by the same clients, so
the difference is the replication underneath. Results are measurements of one host and
configuration: hypotheses to check on your own hardware, not production claims.

```sh
examples/kv-tokio/bench/etcd/run.sh            # everything (about 20 minutes)
examples/kv-tokio/bench/etcd/run.sh --smoke    # short runs, to check the harness
examples/kv-tokio/bench/etcd/run.sh crash partition
```

Needs Docker, `jq`, and cgroup v2 with the `io` controller for the slow-disk scenario. It
builds one image (`Dockerfile`: `kv-tokio`, `kv-bench`, etcd's `benchmark` built from
v3.7.2, and `iproute2`/`iptables`) and pulls etcd's official image by digest. Each run
writes to `target/bench-etcd/<time>/`: `env.txt` (host, versions, measured `fsync`
latency), one file per run, request samples (`*.csv`), and `summary.md`.

## What runs

| Scenario | Load | What happens to the leader |
|---|---|---|
| `steady` | etcd's `benchmark`: put (unlimited, and at `RATE`), range (linearizable, serializable), txn-put; 256 clients over 16 connections, all three members | nothing |
| `mixed` | open loop at `MIXED_RATE` (40000/s), 90% linearizable ranges and 10% puts, on all three members | nothing |
| `crash` | open loop at `RATE` (5000/s) on the two followers | `SIGKILL` at 10 s |
| `partition` | same | its traffic to and from the followers is dropped from 10 s to 25 s |
| `slow-disk` | same | writes capped at `SLOW_IOPS` (50) IOPS from 10 s to 25 s (cgroup `io.max`) |
| `far-leader` | same | `FAR_MS` (50 ms) added to everything it sends from 10 s to 25 s |

Each fault runs against QuePaxa, etcd with its default timeouts (heartbeat 100 ms, election
1000 ms) and etcd tuned for a fast local network (10 ms, 100 ms). QuePaxa's leader is
replica 0; etcd's is whoever it elects, found from `etcdctl endpoint status`.

## Fairness

- One system runs at a time. Each node has its own container and three cores
  (`NODE_CPUS`), the generator four others (`GEN_CPUS`).
- Data lives on the host disk under `target/`, never tmpfs, and both systems `fsync`.
- In the fault scenarios clients talk only to followers, so what is measured is how the
  cluster copes with its leader, not a client losing its server. Network faults run from a
  privileged sidecar in the leader's network namespace (etcd's image has no shell), the
  same for both systems.
- The open-loop generator (`kv-bench --protocol etcd`) times each request from when it
  was due, so a stall counts in full. It starts at a wall-clock time `run.sh` picks
  (`--start-at`), and faults are timed from the same instant, so a fault lands at
  `--fault-at` on the generator's timeline. etcd's own `benchmark` is closed loop: it is used for
  steady state only, where that does not hide anything and its numbers compare with etcd's
  published ones.

## Reading the fault table

`Longest gap` is the longest time after the fault with no request completing; `Degraded`
is how long, in 250 ms steps from the fault to the end of the load, fewer requests
completed than 80% of the pre-fault rate (a brief stall late in the run adds little, a
cluster that stays slow adds every step); `Errors` are requests the server answered with
an error, which etcd's client would retry. Latency percentiles cover successful requests
only: a request etcd fails after its 7 s internal timeout is an error, not a 7 s sample.

## Across regions (`wan.sh`)

```sh
examples/kv-tokio/bench/etcd/wan.sh            # everything (about 80 minutes)
examples/kv-tokio/bench/etcd/wan.sh --smoke    # one short run per case
examples/kv-tokio/bench/etcd/wan.sh crash jitter
```

Five replicas of each system, one per emulated region (us-east, us-west, eu-west, ap-ne,
sa-east), still on this host. Every replica-to-replica link is delayed by half the round
trip between its regions in each direction (`tc netem`, one HTB class per destination; the
matrix is `wan_leaders`'s in `crates/quepaxa-sim`). Before each run the harness pings every
link and aborts if one is off by more than 10% or 5 ms, and checks that each region's
generator reaches its local replica in under 1 ms.

| Case | What happens to the leader's region |
|---|---|
| `calm` | nothing; also run with the leader in the best-placed region (`calm-best`) |
| `crash` | its replica is killed at 10 s |
| `partition` | its traffic to and from the other replicas is dropped from 10 s to 25 s |
| `slow` | `SLOW_MS` (100 ms) added to everything it sends to the others, 10 s to 25 s |
| `jitter` | `JITTER_MS` (50 ms, normal) of jitter and `LOSS` (1%) on what it sends, 10 s to 25 s |

- **Leader.** A majority is three of five, so a leader waits for its second-nearest
  peer. The leader is pinned to the region where that peer is farthest (ap-ne, 180 ms)
  and, for `calm-best`, nearest (us-east, 80 ms): QuePaxa through `--leader-order` (the
  rest of the order by the same rank), etcd through `etcdctl move-leader`, confirmed before
  any load. The summary records which region led after the fault.
- **Timers.** QuePaxa runs with `--retransmit-ms 400 --hedge-max-ms 3000`, the WAN values
  in `quepaxa::Config`'s docs. etcd runs with its defaults (100 ms, 1000 ms) and tuned per
  its guide: heartbeat at the largest round trip (300 ms), election timeout ten times it.
- **Load.** `RATE` (1000 writes/s) split evenly over a generator per region, each with its
  own seed, writing only to its region's replica.
- **Runs.** Each case runs `REPEATS` (3) times on fresh containers; `summary.md` gives the
  median and the range over runs, over all regions (percentiles from the merged samples)
  and per region.
- **Jitter.** netem reorders jittered packets unless the link has a rate, so every link
  carries one far above the load. A rate keeps packets in order by holding each one
  behind the packet ahead, so under load the added delay skews toward the high end of
  the jitter rather than spreading ±50 ms: the case is mostly extra, uneven delay plus
  loss. The harness counts TCP reordering events on every replica during the run
  (`reorders=` in `*-leader.txt`); a handful come from the loss.
- **Faults change links in place** (`tc qdisc change`): replacing a link's qdisc would
  drop the packets queued in it.

It is still one host: links are emulated, there is no bandwidth limit, and every replica
shares the same disk and CPU pool.

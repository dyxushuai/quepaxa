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
  was due, so a stall counts in full. etcd's own `benchmark` is closed loop: it is used for
  steady state only, where that does not hide anything and its numbers compare with etcd's
  published ones.

## Reading the fault table

`Longest gap` is the longest time after the fault with no request completing; `Degraded`
is how long, in 250 ms steps from the fault to the end of the load, fewer requests
completed than 80% of the pre-fault rate (a brief stall late in the run adds little, a
cluster that stays slow adds every step); `Errors` are requests the server answered with
an error, which etcd's client would retry. Latency percentiles cover successful requests
only: a request etcd fails after its 7 s internal timeout is an error, not a 7 s sample.

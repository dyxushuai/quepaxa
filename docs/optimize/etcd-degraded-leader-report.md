# QuePaxa versus etcd: degraded-leader measurements

Runtime source measured on 2026-11-03: `617ee8f7cf6ad99cd52e55edcccee6c01caf5fe6`
(the main revision after PR #132). This is a continuation of the
[etcd comparison](../benchmarks.md#against-etcd), with fresh measurements and
separate diagnostic runs. No protocol or persistence change was retained.

## Build-input correction

The Docker build context originally included `.context/`, where the local
measurement wrapper stores each result. Updating those artifacts invalidated
`COPY . .` and rebuilt the workload on the next run. Binary inspection found
different `kv-tokio` hashes in all three initial repetitions, although the
runtime source revision and `kv-bench` hash were unchanged. This does not
establish that rebuild variation caused the observed tail latency.

The retained change excludes `.context` in the benchmark's Docker ignore file.
It changes build inputs only: workloads, fault timing, etcd image, toolchains,
and runtime sources are unchanged. The initial measurements below remain
exploratory results; a new baseline uses a verified identical binary in each run.

## Fixed-binary three-node baseline

Three full repetitions after the build-input correction, with the same
workload described below. The [build receipts](etcd-degraded-leader-builds.csv)
record each image ID, both binary hashes, and preflight storage timing.

| System | p99, median (range), ms | Longest gap, median (range), ms | Errors across three runs |
|---|---|---|---|
| QuePaxa | 13.141 (13.033–33.810) | 26 (25–35) | 0 |
| etcd, default | 165.771 (161.779–496.468) | 55 (55–94) | 0 |
| etcd, tuned | 915.596 (606.600–1,014.372) | 668 (98–735) | 10,172 |

QuePaxa and default etcd complete all 199,375 requests in each repetition;
all systems report zero unanswered requests and missed arrivals. Tuned etcd
fails 3,760, 2,418, and 3,994 requests, excluded from its latency percentiles.
The three QuePaxa gaps meet the 100 ms median-gap target in this sample.
The p99 spread still shows host or run variation; three repetitions are not
a production bound.

Raw directories are `20261103-230909/`, `20261103-231205/`, and
`20261103-231435/` under `target/bench-etcd/`. All use image
`sha256:2fbfcd8d01bcb33306f81908b2f93e098b384db88c4752871cbd08b435c1ea8a`.
Their replica and generator SHA-256 hashes match. After creating another
scratch artifact, a separate Docker build reports the Rust build step as
`CACHED` and produces the same image ID.

This validates build reuse and establishes a fresh baseline. It does not
attribute the difference from the initial runs to the ignore rule or to a
protocol improvement: build identity and host conditions were not controlled
between those two groups, and the runtime source did not change.

## Initial three-node comparison

Three full runs of the existing `run.sh far-leader` workload: 5,000 writes/s,
16 connections to the two followers, 40 seconds, +50 ms leader egress delay
from 10 to 25 seconds. Each run uses the same Poisson arrival sequence:
199,375 offered requests. Systems run sequentially, not concurrently.

| System | p99, median (range), ms | Longest gap, median (range), ms | Errors across three runs |
|---|---|---|---|
| QuePaxa | 13.770 (13.051–15.317) | 103 (28–174) | 0 |
| etcd, default | 195.539 (193.694–239.366) | 75 (55–129) | 0 |
| etcd, tuned | 586.578 (112.009–815.110) | 103 (13–454) | 4,875 |

Every run reports zero unanswered requests and zero missed arrivals. QuePaxa
and default etcd each complete all 199,375 requests in every run. Tuned etcd
fails 3,838, 1,037, and 0 requests respectively; its successful-response
percentiles exclude those errors. Its 13 ms gap in the second run therefore
does not establish better availability.

QuePaxa has substantially lower p99 in this sample, but its longest gap is
still worse than default etcd in two of three runs. The 100 ms target for
the median gap is not met. With only three runs and a 28–174 ms spread, a
20% selection threshold would not by itself establish a stable tail improvement.
The [per-run measurements](etcd-degraded-leader-runs.csv) retain counts and
individual results, including all diagnostic runs. These initial measurements
compare systems, not the causal effect of PR #132: the pre-merge revision
was not rerun under the same conditions.

Raw run directories under `target/bench-etcd/`:

- `20261103-223227/`: QuePaxa gap 28 ms, p99 13.051 ms.
- `20261103-223549/`: QuePaxa gap 174 ms, p99 15.317 ms.
- `20261103-223850/`: QuePaxa gap 103 ms, p99 13.770 ms.

The last two gaps occur at 26.733–26.908 s and 26.487–26.591 s, after
healing at 25 s. Their initial fault-transition gaps are approximately
25 ms. A longest-gap statistic alone does not locate the recovery cost.

## Five-region comparison

One fresh run per system and scenario, using the existing `wan.sh crash jitter`
workload: 1,000 writes/s across five regions, 40 seconds, faults from 10 to
25 seconds, leader in ap-ne. The RTT matrix ranges from 70 to 300 ms; jitter
adds 50 ms standard deviation and 1% loss on the leader's links. WAN timers
are unchanged: QuePaxa retransmit 400 ms / hedge cap 3 s; etcd default
100/1,000 ms and tuned 300/3,000 ms heartbeat/election intervals.

Crash rows cover only the four surviving regions. Jitter rows cover all five.
The p99 and gap are the **maximum per-region values**, not pooled percentiles;
errors are summed over the included regions.

| Scenario | System | Worst-region p99, ms | Longest gap, ms | Errors |
|---|---|---|---|---|
| Crash | QuePaxa | 1,408.590 | 852 | 0 |
| Crash | etcd, default | 2,623.601 | 1,851 | 1,008 |
| Crash | etcd, tuned | 4,668.950 | 4,430 | 3,098 |
| Jitter + loss | QuePaxa | 1,185.425 | 721 | 0 |
| Jitter + loss | etcd, default | 6,838.350 | 7,427 | 2,683 |
| Jitter + loss | etcd, tuned | 6,950.206 | 4,694 | 0 |

Included regions report zero unanswered requests and missed arrivals.
The [region-level data](etcd-degraded-leader-wan.csv) also preserves the
excluded crashed region: QuePaxa has 132 errors and 5,696 unanswered requests
there. Zero errors in surviving regions does not mean clients attached to a
dead replica remain available.

Raw results are in `target/bench-etcd/wan-20261103-224756/`, including the
link checks and leader checks. One run cannot establish a stable distribution
or quantify a regression against older measurements. No candidate code was
selected, so this is a fresh WAN comparison at the same source revision,
not a baseline-to-candidate holdout comparison. It does not support a new
optimization gain.

## Attribution and optimization decision

Five extra runs are excluded from the comparison above:

- `20261103-224153/` sampled replica `STATS`. Its gap was 60 ms. The initial
  unprivileged syscall trace failed, so this run supplies no disk-duration evidence.
- `20261103-224449/` sampled `STATS` and traced `fsync`/`fdatasync` on the
  owned benchmark processes with existing `sudo` access. Its gap was 104 ms
  and p99 21.187 ms. Tracing overhead makes these unsuitable score samples.
- `20261103-230038/` used kernel tracepoints via `perf trace` and sampled
  `STATS`: gap 358 ms, p99 633.597 ms, zero errors. It observed repeated hedge
  bursts and sync calls lasting up to 177.444 ms. A later calibration found
  the default trace clock differed from the request clock; their exact
  temporal relationship is not established by this recording.
- `20261103-230305/` repeated the same QuePaxa workload without a probe:
  gap 21 ms, p99 13.198 ms, zero errors. One quiet run does not rule out an
  intermittent problem or prove that instrumentation caused the earlier stall.
- `20261103-230539/` explicitly used `perf record -k mono` for sync entry/exit
  events, after validating that clock with a test process: gap 13 ms, p99
  12.732 ms, zero errors. Across 51,080 matched sync calls, the longest lasted
  9.646 ms. The large stall did not recur, so this run does not confirm its cause.

The last three diagnostics reuse the already built image and run only QuePaxa,
with the same request load, fault timing, CPU sets, and data location. They are
not added to the initial three-run set or presented as improvements.

In the second diagnostic, the successful-completion gap spans
25.416346–25.520523 s (104.177 ms). Three writer threads were simultaneously
blocked in `fdatasync`. The [trace excerpt](etcd-degraded-leader-trace.txt)
preserves the syscall timestamps, adjacent request samples, and replica counters.

| Thread | Sync start, s | Sync return, s | Duration, ms |
|---|---|---|---|
| 40602 | 25.414890 | 25.518531 | 103.641 |
| 40537 | 25.415903 | 25.519550 | 103.647 |
| 40483 | 25.415915 | 25.519672 | 103.757 |

The next successful completion arrives 0.851 ms after the last sync returns.
Replica 2 hedges 473 entries around the stall, but this observation does not
show substantial additional delay after the simultaneous sync waits. The adjacent
snapshot/compaction counter drops occur around 20.5 and 27.6 s, not at the
stalled syncs. This is temporal attribution for one instrumented run; it does
not prove the cause of every untraced outlier or identify which host activity
delayed the disk. Syscall wall times include scheduling and tracing overhead;
the device's service time was not measured independently.

Two candidate changes were considered and not implemented:

- **Limit simultaneous hedges.** Less than 1 ms of post-sync recovery remains
  in the strace-observed stall. The later 358 ms episode leaves possible hedge
  amplification unresolved: no aligned trace isolates its contribution, and
  the calibrated repeat does not reproduce it. Expected benefit remains unknown.
- **Reschedule snapshots or compaction.** No observed overlap supports this
  explanation for the traced stall. Snapshot scheduling remains a separate
  hypothesis for other workloads.

There is no retained runtime optimization, no before/after runtime gain, and
no changed durability or consistency contract. Before changing hedge policy, reproduce
the larger episode with an aligned trace and controlled storage-delay workload.
Independent replica disks would help separate shared-storage stalls from
protocol amplification; none were provisioned in this run.

## Reproduction and evidence limits

The host has 16 vCPUs and one shared ext4 volume. Node CPU sets are `0-2`,
`3-5`, and `6-8`; the generator uses `9-12`. Mean time for a 512-byte append
and `fsync`, measured before each run, is 1.691–1.838 ms across the fixed-binary
baseline and 1.688–1.837 ms across the initial three runs. It ranges from
1.705 to 10.437 ms across the five diagnostics;
the 10.437 ms preflight was followed by the quiet 21 ms-gap run, so this
short preflight alone does not predict the workload tail. Data is not on
tmpfs. No competing benchmark
or correctness build was launched during measurement; other host workloads
and storage contention are not controlled.

etcd is pinned to v3.7.2:
`gcr.io/etcd-development/etcd:v3.7.2@sha256:7c6c239825d00e3f6328a69caafd54be92063acf0c2ce78b8394699f52b75dc3`.
Default heartbeat/election intervals are 100/1,000 ms; the local tuned
configuration uses 10/100 ms. QuePaxa uses its existing example configuration.

```sh
RATE=5000 NODE_CPUS='0-2 3-5 6-8' GEN_CPUS=9-12 FAR_MS=50 \
  bash examples/kv-tokio/bench/etcd/run.sh far-leader

RATE=1000 REPEATS=1 REPLICA_CPUS='0-1 2-3 4-5 6-7 8-9' \
  GEN_CPUS=10-15 JITTER_MS=50 LOSS=1 \
  bash examples/kv-tokio/bench/etcd/wan.sh crash jitter
```

Run the first command three times sequentially. Preserve each `env.txt`, summary,
per-system `.txt`, and request-level `.csv`; do not average away failed runs.
The local experiment ledger is
`.context/compound-engineering/ce-optimize/etcd-degraded-leader/experiment-log.yaml`.
Performance completion counts are not a linearizability test. No Rust source
changed, so no new correctness-suite result is claimed here.

## Run outcome

Approximately 45 minutes of local measurement and diagnosis (14:32–15:17 UTC): three initial
LAN repetitions, five diagnostic runs, one WAN run per system/scenario, and
three corrected LAN repetitions. Two runtime hypotheses were not selected;
zero runtime candidates were implemented or retained. One build-input fix
was retained and verified through repeated image identity and a cache check.

The run stops with the measurement correction complete and no isolated
runtime gain to claim. The fixed-binary baseline meets its median-gap target;
the intermittent larger tails and possible hedge amplification remain unresolved.
No new dependency or infrastructure was provisioned.

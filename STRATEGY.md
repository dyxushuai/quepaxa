---
name: quepaxa
last_updated: 2026-10-31
---

# quepaxa Strategy

## Purpose

Systems that must agree on small control-plane state across datacenters are left with
Raft-family libraries, whose availability hangs on election timeouts that have no good value
on a wide-area network: short ones churn leaders under jitter, long ones stall writes when a
leader really fails. QuePaxa removes the timeout from liveness, but no embeddable,
production-grade implementation of it is available (Cloudflare's Meerkat is internal).

## Positioning

The embeddable QuePaxa engine: a sans-IO, `no_std` library whose correctness is proved and
checked continuously, so a service built on it gets liveness without timeouts or elections
while keeping its own network, storage and runtime.

## Users

**Primary:** Developers of cross-region coordination services (control-plane metadata
stores, lock and lease services) - they hire quepaxa as the consensus engine underneath.

**Secondary:** Rust and C developers embedding a replicated state machine in their own system.

## Boundaries

- No service code here: client APIs, storage engines and transports belong to the services
  built on quepaxa; `examples/kv-tokio` stays a reference runtime and benchmark, with no new
  etcd features.
- One consensus group; no sharding or multi-group orchestration, and no general-purpose
  database ambitions.
- `quepaxa-ffi` and the prebuilt platform libraries are maintained (released, CI green), not
  extended.
- Single-datacenter throughput only needs to be good enough; no optimizing for benchmark
  numbers.

_Resist a change when:_ it serves general database workloads, single-datacenter benchmarks or
service features rather than the engine's cross-region availability.

## Key metrics

- **Client errors during faults** - errors clients see while a leader is killed, partitioned
  or slowed; target 0. Measured by the fault scenarios in `examples/kv-tokio/bench/etcd`.
- **Cross-region tail latency** - write p99 and longest stall across 3-5 regions with the
  leader's region degraded, against tuned etcd. Needs a multi-region harness (not built yet).
- **Verification coverage** - every protocol feature has a model or proof plus canaries, and
  every canary is caught. Measured by `VERIFICATION.md`'s model-to-code rows and
  `verification/canaries.sh`.
- **Open engine blockers** - issues that keep a service built on quepaxa out of production.
  Measured in the GitHub tracker.

## Tracks

### Cross-region liveness and tail latency

Make leader faults and distant leaders cost little latency: a multi-region test harness,
leader placement that follows load, and hedging tuned for wide-area links.

_Why it serves the approach:_ it is the claim only a timeout-free engine can make, and today's
far-leader numbers do not back it up yet.

### Engine features for production services

The APIs a production service needs from the engine: membership bootstrap and management,
snapshot and storage hooks, metrics and observability.

_Why it serves the approach:_ a service can only reach production on what the engine
exposes, without the library taking on service code.

### Correctness assurance

TLA+/TLAPS/Verus models, simulation and canaries keep pace with every protocol change.

_Why it serves the approach:_ a consensus library is only embeddable if its users can trust it
without verifying it again themselves.

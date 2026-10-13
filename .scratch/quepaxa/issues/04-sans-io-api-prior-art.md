# How do existing Rust consensus crates shape a sans-IO API

Parent: [map](../map.md)
Type: research
Status: open
Blocked by: 

## Question

Study API shapes of raft-rs (Ready/RawNode), openraft, omnipaxos, and any other sans-IO protocol crates (quinn-proto, rustls) for: input events vs output actions, time and randomness injection, storage traits and persistence ordering guarantees, batching, error handling, no_std support. Extract lessons and pitfalls relevant to QuePaxa.

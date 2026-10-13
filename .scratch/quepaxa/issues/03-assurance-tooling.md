# Which tools give us model checking, DST and linearizability checking in Rust

Parent: [map](../map.md)
Type: research
Status: open
Blocked by: 

## Question

Survey and recommend: model checking/formal methods for a sans-IO Rust state machine (stateright, TLA+/Apalache incl. any existing QuePaxa TLA+ spec, Kani, Verus, P), deterministic simulation (madsim, turmoil, hand-rolled), linearizability checkers (porcupine-style, stateright's, knossos), fuzzing (cargo-fuzz/libFuzzer, AFL++, bolero, structure-aware fuzzing of message sequences), coverage (cargo-llvm-cov). For each: maturity, no_std friendliness, CI cost, how it would fit. Recommend one stack.

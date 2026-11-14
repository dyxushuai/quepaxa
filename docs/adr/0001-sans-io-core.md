# Sans-IO deterministic core

The `quepaxa` crate is a pure state machine: it never touches the network, disk, clock, or OS randomness; callers feed it input events (messages, time, proposals, storage completions) and execute the output actions it returns. We chose this over an async runtime-bound core because it makes deterministic simulation, fuzzing and model checking possible on the real code, keeps the core `no_std + alloc` for embedded/wasm targets, and keeps the C ABI (`quepaxa-ffi`) a thin marshalling layer instead of a second runtime.

The reference runtime around this core (one thread orders, another applies) is described in [ADR 0005](0005-kv-tokio-runtime-threads.md).

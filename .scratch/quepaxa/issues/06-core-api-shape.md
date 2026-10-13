# What is the core sans-IO API

Parent: [map](../map.md)
Type: grilling
Status: open
Blocked by: 01, 02, 04

## Question

Decide the core crate's public surface: input events (messages, ticks/time, proposals, storage completions), output actions (send, persist, decided entries, timers), how time and randomness are injected, the storage contract and persistence ordering, read_barrier semantics, configuration knobs, error model, and no_std+alloc constraints.

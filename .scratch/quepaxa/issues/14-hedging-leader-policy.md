# How are the hedging delay and leader order chosen

Parent: [map](../map.md)
Type: grilling
Status: open
Blocked by: 06

## Question

The paper gives no δ-tuning algorithm and the Go impl uses a static δ plus an epoch-based leader-order tuner agreed through a log pseudo-command. Decide: the policy trait boundary in the core, the default leader-order tuner (and whether its agreement rides the log), how δ is set or adapted, what counts as evidence that suppresses a hedged proposer (per step vs per slot), and how each is tested deterministically.

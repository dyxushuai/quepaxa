# What is the wire format and its compatibility policy

Parent: [map](../map.md)
Type: grilling
Status: open
Blocked by: 01, 06

## Question

Decide message encoding (hand-rolled, postcard, prost, etc.) or leaving encoding to callers, versioning, and the compatibility promise across crate versions and across the C ABI.

Note (from the phase-one scope answer): every message must carry `cluster_id` and `config_epoch`, and slot numbers are scoped per epoch, so phase-2 reconfiguration needs no wire break.

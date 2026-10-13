# What is the wire format and its compatibility policy

Parent: [map](../map.md)
Type: grilling
Status: resolved
Blocked by: 01, 06

## Question

Decide message encoding (hand-rolled, postcard, prost, etc.) or leaving encoding to callers, versioning, and the compatibility promise across crate versions and across the C ABI.

Note (from the phase-one scope answer): every message must carry `cluster_id` and `config_epoch`, and slot numbers are scoped per epoch, so phase-2 reconfiguration needs no wire break.

## Answer

Resolved 2026-10-13 under the "一步到位" directive.

1. **The core owns both encodings**: network messages (`Message::encode` / `Message::decode`) and durable records (`WriteOp` bytes). Callers and FFI users treat both as opaque bytes; framing on a stream is the transport's job (one message = one frame).
2. **Hand-rolled codec**, no serde/prost: little-endian fixed-width integers, `u32`-length-prefixed byte fields, payloads decoded as zero-copy `Bytes` slices. Every length is checked against `Config` limits (`max_message_bytes`, default 16 MiB; `max_batch_bytes`) **before** allocation (Go trusted lengths; we don't).
3. **Envelope**: magic `"QP"`, `wire_version: u8`, `kind: u8`, `cluster_id: [u8;16]`, `config_epoch: u64`, `from: u32`, `to: u32`, `commit_index: u64`. Mismatched cluster/epoch/version or unknown kind → drop and count.
4. **Kinds (v1)**: `RecordRequest {slot, step, proposal}`, `RecordReply {slot, S, Fc, Ap}` | `Decided {slot, value}` | `Compacted {through}`, `CatchUp {from_slot}`, `Entries {first_slot, values}`, `Forward {proposals}`.
5. **Proposal**: `priority: u64`, `value_key: (origin u32, incarnation u64, batch_seq u64)`, `value: kind u8 (0 = inline batch) + bytes`. Replies elide the value bytes of any proposal whose `value_key` equals the request's (the proposer already holds it).
6. **Integrity**: no checksum on the wire (CFT; transport is reliable-or-dropping). Durable records carry CRC32C (software table, no new dependency).
7. **Compatibility**: during 0.x, `wire_version` must match exactly. From 1.0: a release interoperates with the previous minor's wire version (rolling upgrade). Durable format has its own version: newer code reads every older version, migrations forward-only, never older-reads-newer. Golden-file tests pin every version's bytes.

//! [QuePaxa](https://bford.info/pub/os/quepaxa/quepaxa.pdf) consensus for state machine
//! replication: a sans-IO, `no_std` replicated log, and an alternative to Raft and
//! Multi-Paxos that never stalls waiting for an election timeout.
//!
//! A cluster of replicas agrees on one ordered log of opaque entries, tolerating the crash
//! (and recovery) of any minority. QuePaxa's distinguishing property is that **liveness
//! never depends on timeouts**: a leader gives a one-round-trip fast path, but when it is
//! slow, partitioned or gone, any replica can drive any slot with randomized priorities and
//! the log keeps moving — no election, no stall waiting for a timeout to fire.
//!
//! # Sans-IO
//!
//! A [`Replica`] never touches the network, a disk or a clock, and owns no threads. You feed
//! it events and carry out the [`Action`]s it returns: persist these records, send these
//! messages, apply this slot. That makes the same code run under Tokio, a custom event loop,
//! a deterministic simulator or a C program (see the `quepaxa-ffi` crate), and it is why the
//! crate is `no_std` (with `alloc`).
//!
//! # Quick start
//!
//! A single-member cluster commits on its own:
//!
//! ```
//! use bytes::Bytes;
//! use quepaxa::{Action, Config, Input, NodeId, Replica, Timestamp};
//!
//! let cfg = Config::new([7; 16], NodeId(0), vec![NodeId(0)], [42; 32]);
//! let mut replica = Replica::new(cfg, [], Timestamp(0))?;
//! let mut disk = Vec::new();
//! let mut actions = Vec::new();
//!
//! replica.poll_actions(&mut actions); // after every call, before the next one
//! replica.propose(Timestamp(0), Bytes::from_static(b"hello"))?;
//! replica.poll_actions(&mut actions);
//!
//! let mut committed = Vec::new();
//! while !actions.is_empty() {
//!     for action in std::mem::take(&mut actions) {
//!         match action {
//!             // Durable first (append + fsync), then `then` messages, then report.
//!             Action::Persist { id, records, then, .. } => {
//!                 disk.extend(records);
//!                 assert!(then.is_empty());
//!                 replica.handle(Timestamp(0), Input::Persisted(id))?;
//!             }
//!             Action::Send(_out) => { /* deliver out.bytes to out.to */ }
//!             Action::Commit { entries, .. } => committed.extend(entries.into_iter().map(|e| e.payload)),
//!             _ => {}
//!         }
//!     }
//!     replica.poll_actions(&mut actions);
//! }
//! assert_eq!(committed, [Bytes::from_static(b"hello")]);
//! # Ok::<(), quepaxa::Error>(())
//! ```
//!
//! # Where to go next
//!
//! The [`guide`] explains each part of an embedding in its own chapter:
//! [driving the replica](guide::driving), [durability and restart](guide::durability),
//! [snapshots](guide::snapshots), [linearizable reads](guide::reads),
//! [membership and leaders](guide::membership), [tuning](guide::tuning) and
//! [how the protocol works](guide::protocol). Runnable programs are in the crate's
//! `examples/` directory; `examples/kv-tokio` in the repository is a complete networked,
//! `fsync`ed key-value store with a load generator.
//!
//! # Guarantees
//!
//! - **Agreement and validity:** every replica applies the same entries in the same order,
//!   and only entries that were proposed; each entry at most once.
//! - **Crash recovery:** replies never leave before the state they report is durable, and a
//!   restarted replica never contradicts what it said before — given storage that keeps
//!   the promises in [`guide::durability`].
//! - **Linearizable reads** through [read barriers](guide::reads).
//! - **Liveness** whenever a majority of the members can communicate, without depending on
//!   timeouts or a stable leader.
//!
//! These are checked by unit and property tests, a deterministic simulator with crashes,
//! partitions and membership changes, model checking of the real replica, fuzzing, mutation
//! testing, seeded-bug canaries and an independent TLA+ model (`VERIFICATION.md` in the
//! repository). The crate assumes crash faults: peers are not malicious, so authenticate
//! the network yourself if it is not trusted.
//!
//! # Features and platforms
//!
//! No default features; the `model` feature is test support and not part of the API. The
//! core builds for any target with `alloc`, including `wasm32-unknown-unknown` and
//! `thumbv7em-none-eabihf`; it is tested on Linux, macOS and Windows. Minimum Rust: 1.89.
//!
//! The wire format, durable format and API may change in minor versions before 1.0; the
//! changelog lists every such change with upgrade notes.
#![no_std]
#![warn(clippy::missing_errors_doc, clippy::missing_panics_doc)]
// A replica must never take its host down: failures are `Error`s or fencing.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

extern crate alloc;

mod api;
mod batch;
mod codec;
mod core_state;
mod crc;
mod dedup;
mod durable;
pub mod guide;
#[cfg(feature = "model")]
#[doc(hidden)]
pub mod inspect;
mod proposal;
mod proposer;
mod recorder;
mod replica;
mod rng;
mod schedule;
#[cfg(test)]
mod slot_sim;
mod types;

pub use api::{Action, Config, DropReason, Error, Input, Metrics, Outbound};
pub use batch::Entry;
pub use replica::Replica;
pub use types::{NodeId, PersistId, ProposalId, Slot, Timestamp};

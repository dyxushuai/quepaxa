//! Guides to embedding a replica correctly, one topic per chapter.
//!
//! Read [`driving`] first: it is the contract every other chapter builds on.
//!
//! | Chapter | Read it to |
//! |---|---|
//! | [`driving`] | write the event loop: calls, actions, persist-before-send, timers |
//! | [`durability`] | store records and restart safely |
//! | [`snapshots`] | bound the log with application snapshots; catch up from a peer's |
//! | [`reads`] | serve linearizable reads |
//! | [`membership`] | choose the leader, add and remove replicas |
//! | [`tuning`] | set limits and timers, for LANs and wide-area clusters |
//! | [`protocol`] | understand what the replica is doing (QuePaxa in brief) |
//!
//! The code in these chapters runs as tests. Complete programs live in the crate's
//! `examples/` directory (`cargo run -p quepaxa --example cluster`, `restart`,
//! `membership`).

#[doc = include_str!("../docs/driving.md")]
pub mod driving {}

#[doc = include_str!("../docs/durability.md")]
pub mod durability {}

#[doc = include_str!("../docs/snapshots.md")]
pub mod snapshots {}

#[doc = include_str!("../docs/reads.md")]
pub mod reads {}

#[doc = include_str!("../docs/membership.md")]
pub mod membership {}

#[doc = include_str!("../docs/tuning.md")]
pub mod tuning {}

#[doc = include_str!("../docs/protocol.md")]
pub mod protocol {}

//! QuePaxa crash-fault-tolerant consensus as a sans-IO, `no_std` replicated log.
//!
//! A [`Replica`] never touches the network, disk or clock. The caller feeds it events and
//! carries out the [`Action`]s it returns:
//!
//! 1. Call one of [`Replica::propose`], [`Replica::handle`] or [`Replica::handle_timeout`].
//! 2. Drain [`Replica::poll_actions`] **before the next call**, and execute the actions in
//!    order: append each [`Action::Persist`]'s records durably (after all earlier ones),
//!    only then send its `then` messages and report [`Input::Persisted`].
//! 3. Re-arm a timer for [`Replica::next_deadline`].
//!
//! On restart, pass every durable record back to [`Replica::new`] in write order, with a
//! **fresh** CSPRNG seed in the [`Config`]: liveness relies on priorities the network
//! cannot predict.
//!
//! ```
//! use bytes::Bytes;
//! use quepaxa::{Action, Config, Input, NodeId, Replica, Timestamp};
//! use std::collections::VecDeque;
//!
//! let cfg = Config::new([7; 16], NodeId(0), vec![NodeId(0)], [42; 32]);
//! let mut replica = Replica::new(cfg, [], Timestamp(0))?;
//! let (mut disk, mut committed) = (Vec::new(), Vec::new());
//! let (mut actions, mut queue) = (Vec::new(), VecDeque::new());
//!
//! replica.poll_actions(&mut actions);
//! queue.extend(actions.drain(..));
//! replica.propose(Timestamp(0), Bytes::from_static(b"hello"))?;
//! replica.poll_actions(&mut actions);
//! queue.extend(actions.drain(..));
//!
//! while let Some(action) = queue.pop_front() {
//!     match action {
//!         Action::Persist { id, records, then } => {
//!             disk.extend(records); // append + fsync in a real system
//!             assert!(then.is_empty()); // a single member has nobody to message
//!             replica.handle(Timestamp(0), Input::Persisted(id))?;
//!             replica.poll_actions(&mut actions);
//!             queue.extend(actions.drain(..));
//!         }
//!         Action::Commit { entries, .. } => committed.extend(entries.into_iter().map(|e| e.payload)),
//!         _ => {}
//!     }
//! }
//! assert_eq!(committed, [Bytes::from_static(b"hello")]);
//! # Ok::<(), quepaxa::Error>(())
//! ```
#![no_std]

extern crate alloc;

mod api;
mod batch;
mod codec;
mod crc;
mod dedup;
mod durable;
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

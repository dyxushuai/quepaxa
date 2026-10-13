//! QuePaxa crash-fault-tolerant consensus as a sans-IO, `no_std` replicated log.
//!
//! See the v1 spec in the repository for the protocol and API contract.
#![no_std]
// shortcut: protocol pieces land before `Replica` wires them up; drop once #9 lands.
#![allow(dead_code)]

extern crate alloc;

mod codec;
mod proposal;
mod proposer;
mod recorder;
mod rng;
#[cfg(test)]
mod slot_sim;
mod types;

pub use types::{NodeId, Slot};

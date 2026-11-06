//! The `std::sync` and `std::thread` items the node's threads share. With the `shuttle`
//! feature they are shuttle's, whose schedulers then explore how those threads interleave
//! (`node/shuttle_tests.rs`); the binary built that way is for those tests only.

#[cfg(feature = "shuttle")]
pub use shuttle::{
    sync::{Arc, Condvar, Mutex, MutexGuard, mpsc},
    thread,
};
#[cfg(not(feature = "shuttle"))]
pub use std::{
    sync::{Arc, Condvar, Mutex, MutexGuard, mpsc},
    thread,
};

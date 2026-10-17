//! loadmaster: weighted job admission for parallel AI agents.
//!
//! The core (configuration, queue, process tracking, task supervision) lives
//! in this library; the `loadmaster` binary is a thin CLI on top of it.

pub mod check;
pub mod config;
pub mod doctor;
pub mod error;
pub mod history;
pub mod process;
pub mod queue;
pub mod report;
pub mod runner;
pub mod usual;

pub use error::{Error, Result};

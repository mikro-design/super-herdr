//! The Super-Herdr daemon: the federation authority, and the clients it serves.
//!
//! [`broker`] holds the rules and is pure. [`server`] performs the I/O those
//! rules ask for and reports what happened back through the same broker.

pub mod broker;
pub mod server;
pub mod web;

pub use broker::{Broker, ClientId, Effect};
pub use server::{DaemonOptions, serve};

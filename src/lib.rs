//! Reusable Tapedrive-backed Git transport.
//!
//! The remote-helper binary is one adapter over this crate. Services can use
//! the readiness probe directly without invoking Git's line protocol.

mod fetch;
mod git;
mod index;
mod probe;
mod publication;
mod push;
mod store;

pub mod remote_helper;

pub use git::Repository;
pub use probe::{Cloneability, probe_cloneable};
pub use publication::{PublishedRepository, Publisher, Verification};

//! Git on Tapedrive.
//!
//! The `git-remote-tape` binary is a thin adapter over [`remote_helper`]. The
//! [`index`] module is the on-tape format that binary reads and writes, for
//! tooling that publishes or inspects a repository without going through Git.

mod fetch;
mod git;
mod push;
mod store;

pub mod index;
pub mod remote_helper;

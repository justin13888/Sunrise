//! Library surface of the Sunrise command-line client.
//!
//! The subcommands themselves live in the binary, where their stdout is their
//! contract. What is here is the part an integration test has to drive without
//! a process boundary: [`livesync`], the env→plan→running-sync-session
//! sequence the binary performs at startup, and [`vault`], which decides what
//! key a vault directory is opened with.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::module_name_repetitions,
    clippy::single_match_else
)]

pub mod livesync;
pub mod login;
pub mod vault;

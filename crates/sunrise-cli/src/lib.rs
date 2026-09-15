//! Library surface of the Sunrise command-line client.
//!
//! The subcommands themselves live in the binary, where their stdout is their
//! contract. What is here is the part an integration test has to drive without
//! a process boundary: [`livesync`], the env→plan→running-sync-session
//! sequence the binary performs at startup, [`vault`], which decides what key a
//! vault directory is opened with, [`recover`], which is the one subcommand
//! whose whole job happens before a vault exists, and [`private_file`], the one
//! owner-only write all of them put their secrets on disk with.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::module_name_repetitions,
    clippy::single_match_else
)]

pub mod livesync;
pub mod login;
pub mod private_file;
pub mod recover;
pub mod vault;

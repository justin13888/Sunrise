//! Library surface of the Sunrise command-line client.
//!
//! Most subcommands live in the binary, where their stdout is their contract.
//! What is here is the part an integration test has to drive without a process
//! boundary: [`livesync`], the env→plan→running-sync-session sequence the
//! binary performs at startup, [`vault`], which decides what key a vault
//! directory is opened with, [`private_file`], the one owner-only write both of
//! them put their secrets on disk with, and [`pair`] — which is here rather
//! than in the binary because it is the one family of subcommands that runs
//! *before* a vault is opened, and because a four-step exchange across two
//! machines is worth driving end to end in one process.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::module_name_repetitions,
    clippy::single_match_else
)]

pub mod livesync;
pub mod login;
pub mod pair;
pub mod private_file;
pub mod vault;

/// Lowercase hex of a 16-byte id, for the device and identity listings.
///
/// These ids have no `EntityRef` prefix — a device id is a BLAKE3 derivation
/// of `D_S_pub` and an identity id one of `ID_S_pub`, neither of them a ULID —
/// so hex is the only form they have ever had on screen or in a log.
#[must_use]
pub fn hex16_public(bytes: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(32);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

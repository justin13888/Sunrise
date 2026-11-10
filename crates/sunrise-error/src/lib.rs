//! Stable canonical error codes shared across the workspace.
//!
//! Per `docs/10-cross-cutting/error-handling.md`, the registry source of
//! truth is `codes.toml`. The Rust enum in [`codes`] is a hand-kept mirror
//! of it, and the test in `tests/manifest_in_sync.rs` fails when a code
//! name is in one and not the other.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod codes;
pub mod kind;

pub use codes::ErrorCode;
pub use kind::ErrorKind;

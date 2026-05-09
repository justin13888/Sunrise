//! End-to-end gating crate.
//!
//! Boots cross-crate scenarios that span [`sunrise-core`] +
//! [`sunrise-server`]. Used by Phase 17 release-gating checks per
//! `spec/10-cross-cutting/testing.md`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Crate-level marker used by the test harness.
pub const E2E_CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

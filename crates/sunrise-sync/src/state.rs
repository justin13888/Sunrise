//! Sync session states per `docs/05-sync/multi-device.md`.
//!
//! ```text
//! Disconnected ──connect ok──> Catching up
//!      ▲                            │
//!      │                         caught up
//!      │                            ▼
//!      └───────────────────────── Live ──disconnect──┐
//!                                                    ▼
//!                                             Disconnected
//! ```
//!
//! The transitions themselves are driven by `sunrise-core::sync_driver`,
//! which owns the connection lifecycle; this module only names the states so
//! the driver, the wire layer, and the UI share one vocabulary.

use serde::{Deserialize, Serialize};

/// Sync session states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// No active session.
    Disconnected,
    /// Session opened; replaying op log to catch up to live tail.
    CatchingUp,
    /// Live; ops flow in real time.
    Live,
}

//! Sync session states per `docs/05-sync/multi-device.md`.
//!
//! ```text
//! Disconnected ──connect ok──> Catching up
//!      ▲                            │
//!      │                         caught up
//!      │                            ▼
//!      └───────────────────────── Live ──disconnect──┐
//!                                    │               ▼
//!                          relay reports a    Disconnected
//!                            missing range           ▲
//!                                    ▼               │
//!                                Degraded ──disconnect┘
//!
//! Any connected state ──terminal Close──> Stopped ──new credential──> Disconnected
//! ```
//!
//! A terminal `Close` is one whose code is not `retryable` in
//! `crates/sunrise-error/codes.toml`.
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
    /// Connected and receiving, but the relay has reported a range of ops it
    /// can no longer produce, so local state is known to be incomplete.
    ///
    /// Distinct from every other state because it is the only one that is not
    /// about the *connection*: the socket is healthy and ops are flowing. It
    /// exists so a client can never answer "am I up to date?" with `Live`
    /// when the relay has already said otherwise. Re-subscribing cannot clear
    /// it — the ops are gone from the relay — so it persists for the rest of
    /// the session and is resolved out of band.
    Degraded,
    /// The relay closed the session for a reason the client cannot recover
    /// from on its own — a revoked device, relay storage that is unavailable,
    /// or a close code this build cannot read — and the driver has stopped
    /// reconnecting.
    ///
    /// Distinct from `Disconnected`, which retries on its own: reconnecting
    /// here would present the same device to a relay that has already said it
    /// will not take it, which is the "retries forever against a revoked
    /// device" loop `docs/05-sync/wire-protocol.md` forbids. The driver leaves
    /// this state when the credential is replaced (the user signed in again)
    /// or the app restarts; both are the user's act, not a timer's.
    Stopped,
}

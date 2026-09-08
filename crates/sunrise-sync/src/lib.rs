//! Sync session vocabulary + transport trait.
//!
//! Per `docs/05-sync/`. This crate holds only the transport-agnostic pieces
//! the sync driver needs; the driver itself — connection lifecycle, cursor
//! tracking, and outbound queueing — lives in `sunrise-core::sync_driver`,
//! and the durable outbox is `sunrise_storage::Outbox`.
//!
//! v1 surface:
//! - [`SyncState`] — the session state a driver reports to the UI.
//! - [`Backoff`] — exponential backoff with jitter.
//! - [`TokenSource`] — the shared, swappable bearer a session presents.
//! - [`Transport`] — async trait the driver drives.
//! - [`SseTransport`] — the production SSE + POST client transport (`sse`
//!   feature).

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions
)]

pub mod backoff;
pub mod credential;
#[cfg(feature = "sse")]
pub mod sse;
pub mod state;
pub mod transport;

pub use backoff::Backoff;
pub use credential::{TokenSource, TokenWatch};
#[cfg(feature = "sse")]
pub use sse::SseTransport;
pub use state::SyncState;
pub use transport::{RevokeOutcome, Transport, TransportError};

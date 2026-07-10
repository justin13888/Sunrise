//! Sync state machine + cursors + outbox + transport trait.
//!
//! Per `docs/05-sync/`. The state machine is transport-agnostic — concrete
//! WebSocket / HTTP-long-poll transports plug in via the [`Transport`]
//! trait. Phase 11 (server) and the per-platform clients in Phase 13+ will
//! provide implementations.
//!
//! v1 surface:
//! - [`SyncState`] state machine.
//! - [`Cursor`] per `(stream_id, originating_device_id, last_applied_seq)`.
//! - [`Outbox`] FIFO of outbound op envelopes.
//! - [`Backoff`] exponential backoff with jitter.
//! - [`Transport`] async trait the state machine drives.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions
)]

pub mod backoff;
pub mod cursor;
pub mod outbox;
pub mod state;
pub mod transport;
#[cfg(feature = "ws")]
pub mod ws;

pub use backoff::Backoff;
pub use cursor::{Cursor, CursorMap};
pub use outbox::Outbox;
pub use state::{SyncState, SyncStateMachine, SyncStateTransition};
pub use transport::{Transport, TransportError};
#[cfg(feature = "ws")]
pub use ws::WsTransport;

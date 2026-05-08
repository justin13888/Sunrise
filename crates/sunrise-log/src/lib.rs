//! Sunrise's layered NDJSON logging contract.
//!
//! Implements `spec/10-cross-cutting/logging.md`. This crate is the only
//! place in the workspace that writes to a log sink; every other crate emits
//! events through the macros and types re-exported here. Direct calls to
//! `println!`, `eprintln!`, `tracing::info!`, etc. are forbidden in shipped
//! code (lint-enforced via the workspace clippy config).
//!
//! # Layered structure
//!
//! - [`record`] — the wire NDJSON record; mirrors `schemas/log-record.v1.json`.
//! - [`level`] — five levels mapped 1:1 to RFC 5424.
//! - [`plain`] — the [`Plain<T>`] redaction wrapper. Plaintext domain data
//!   only crosses module boundaries inside `Plain<T>`; the logging API
//!   refuses to format it.
//! - [`event`] — the per-package `ev` catalog and event-name validator.
//! - [`ctx`] — the redaction-allowlisted context-key set per logging.md §6.
//! - [`sink`] — output transports: stderr, file, ring, remote.
//! - [`throttle`] — per-`(ev, lv)` token bucket rate limit (logging.md §9).
//! - [`init`] — `init(LogConfig)` bootstrap.
//! - [`span`] — task-local trace/span propagation (logging.md §4).
//!
//! Public macros live at the crate root: [`event!`], [`error_with!`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod ctx;
pub mod event;
pub mod init;
pub mod level;
// `macros` declares `#[macro_export]` macros which become `crate-root::name`
// automatically; no `pub use` needed.
mod macros;
pub mod plain;
pub mod proto;
pub mod record;
pub mod sink;
pub mod span;
pub mod throttle;

pub use ctx::{Ctx, CtxKey, CtxValue};
pub use event::{EventName, EventNameError};
pub use init::{init, install_global, LogConfig, LogConfigBuilder, LogError};
pub use level::Level;
pub use plain::Plain;
pub use proto::ProtoVersions;
pub use record::{ErrField, ErrorKind, Record};
pub use sink::Sink;
pub use span::{SpanId, TraceId};

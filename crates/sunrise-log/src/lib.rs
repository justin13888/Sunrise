//! Sunrise's redaction layer for `tracing`.
//!
//! Structured logging in this workspace is [`tracing`] + `tracing-subscriber`:
//! levels, spans, `#[instrument]`, per-target filtering, JSON formatting and
//! callsite caching all come from there. This crate does **not** reimplement
//! any of it. It supplies the two things `tracing` has no opinion about and
//! an end-to-end-encrypted app cannot get wrong:
//!
//! 1. **[`Plain<T>`]** — a wrapper for plaintext user data that implements no
//!    `Display`, no `serde::Serialize`, and no `tracing::Value`, and whose
//!    `Debug` prints `Plain<…>`. It cannot be formatted into a log record as
//!    anything but that literal string, through any tracing path. Reading the
//!    value needs an explicit `.expose()`, which CI forbids inside logging,
//!    telemetry, and observability surfaces.
//! 2. **[`RedactionLayer`]** — a `tracing` layer that vetoes any event from a
//!    `sunrise_*` target carrying a field name outside the
//!    [`field::ALLOWED`] vocabulary. This is what catches the value that was
//!    `.expose()`d somewhere legitimate and then logged.
//!
//! Around those sit the pieces a subscriber needs and `tracing-subscriber`
//! deliberately leaves to the application: [`init()`] assembles the stack,
//! [`writer`] provides a size-capped file sink and an in-memory test sink,
//! [`time`] stamps records RFC 3339 in UTC, and [`event`] validates the
//! hierarchical `ev` names catalogued in
//! `docs/10-cross-cutting/log-events.md`.
//!
//! # Emitting
//!
//! Use `tracing` directly. Every Sunrise event carries an `ev` field naming
//! it in the catalogue; all other fields must be on the allowlist.
//!
//! ```
//! # fn demo(lat_ms: u64) {
//! tracing::info!(ev = "srv.ws.connect", lat_ms, "relay session opened");
//! # }
//! ```
//!
//! # Installing
//!
//! ```no_run
//! // A server logs NDJSON to stderr.
//! sunrise_log::init_stderr().expect("logger");
//!
//! // A full-screen terminal app logs to a file — stderr would corrupt the
//! // alternate screen.
//! let path = sunrise_log::init_file("sunrise-cli").expect("logger");
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod event;
pub mod field;
pub mod init;
pub mod plain;
pub mod proto;
pub mod redact;
pub mod time;
pub mod writer;

pub use event::{is_valid_name, EventName, EventNameError};
pub use field::{is_allowed, templatize_path};
pub use init::{
    build_subscriber, build_subscriber_with, default_log_path, init, init_file, init_file_at,
    init_stderr, LogConfig, LogError, LogFormat, LogTarget,
};
pub use plain::Plain;
pub use proto::ProtoVersions;
pub use redact::{RedactionLayer, ViolationPolicy};
pub use time::Rfc3339Millis;
pub use writer::{Capture, RollingFile};

//! The structured vocabulary, mirrored for the foreign side.
//!
//! # Why mirror rather than declare remotely
//!
//! `#[uniffi::remote(Record)]` would avoid the copies, but it still requires
//! every *field* type to be UniFFI-representable, and three of the domain's
//! shapes are not:
//!
//! * `BTreeSet<EntityRef>` — UniFFI has sequences, not sets. `Task` carries
//!   three of them.
//! * `Unknowns` — the forward-compatibility map. It exists so a field written
//!   by a newer schema survives a round trip through this build; it is not
//!   something a UI reads, and exposing its structure would invite a client to
//!   treat it as data. A mirror that a client hands back whole (a constraint,
//!   a rule, a template, a time) carries it as an opaque `extra` blob of
//!   canonical CBOR instead, which the client passes through and never reads
//!   (ADR-0045 §6, #431).
//! * `[u8; 16]` — op and device ids. UniFFI has no fixed-size array type at
//!   all.
//!
//! One mirror per shape is also what lets [`TaskItem`] be called that. A record
//! named `Task` generates a Swift `struct Task` that shadows
//! `_Concurrency.Task`, so every `Task { … }` in the app stops compiling. That
//! rename has to happen before any UI is written against it.
//!
//! # Drift
//!
//! Every mirror's `From` impl **destructures its source exhaustively** — no
//! `..` rest patterns. Adding a field to a domain entity therefore fails this
//! crate's build, which is the only way a mirror layer stays honest.
//!
//! # Layout
//!
//! One submodule per domain area, each holding that area's mirrors and their
//! conversions; every public item is re-exported here, so a mirror's path is
//! `sunrise_core_bindings::dto::X` whichever file it lives in. `UniFFI` names
//! are global, so the split changes nothing the foreign side sees.

mod account;
mod activity;
mod attachments;
mod blocks;
mod capture;
mod constraints;
mod feature_gate;
mod focus;
mod notifications;
mod preferences;
mod recurrence;
mod registry;
mod review;
mod routine;
mod rows;
mod stream;
mod task;
mod time;

pub use self::account::*;
pub use self::activity::*;
pub use self::attachments::*;
pub use self::blocks::*;
pub use self::capture::*;
pub use self::constraints::*;
pub use self::feature_gate::*;
pub use self::focus::*;
pub use self::notifications::*;
pub use self::preferences::*;
pub use self::recurrence::*;
pub(crate) use self::registry::Mirrored;
pub use self::review::*;
pub use self::routine::*;
pub use self::rows::*;
pub use self::stream::*;
pub use self::task::*;
pub use self::time::*;

/// Format the 16-byte op/device/identity ids UniFFI cannot carry as lowercase
/// hex — the client layer's one spelling, so the app and the CLI agree.
pub(crate) use sunrise_client_core::ids::hex_id as hex16;

/// A nested value's unknown fields, as the opaque blob its mirror carries:
/// canonical CBOR of the map, or `None` when there are none.
///
/// The client never reads the blob; it only hands it back, which is what lets
/// a constraint, rule or template it edits keep the fields a newer build wrote
/// (ADR-0045 §6).
fn extra_out(unknown: &sunrise_domain::Unknowns) -> Option<Vec<u8>> {
    if unknown.is_empty() {
        return None;
    }
    // A map that decoded can be encoded; on the impossible failure the fields
    // are dropped, exactly as the mirror did before it carried them.
    sunrise_cbor::encode_canonical(unknown).ok()
}

/// The inverse of [`extra_out`]. Absent, or bytes that are not a CBOR map
/// (only a client that built the blob itself can send those), read as no
/// unknown fields.
fn extra_in(extra: Option<&[u8]>) -> sunrise_domain::Unknowns {
    extra
        .and_then(|bytes| sunrise_cbor::decode_lenient(bytes).ok())
        .unwrap_or_default()
}

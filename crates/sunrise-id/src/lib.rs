//! Sunrise identifiers.
//!
//! Implements `spec/02-domain/identifiers.md`: ULIDs (128-bit, Crockford
//! base-32, 26 chars) namespaced by entity kind. The 16 raw bytes are the
//! authoritative form; the prefixed string `"tsk_…"` is the I/O-boundary
//! representation.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod crockford;
pub mod entity_ref;
pub mod kind;
pub mod ulid;

pub use crockford::{decode_str, encode_bytes, CrockfordError};
pub use entity_ref::{EntityRef, EntityRefError};
pub use kind::EntityKind;
pub use ulid::{Ulid, UlidParseError};

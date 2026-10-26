//! Sunrise identifiers.
//!
//! Implements `docs/02-domain/identifiers.md`: ULIDs (128-bit, Crockford
//! base-32, 26 chars) namespaced by entity kind. The 16 raw bytes are the
//! authoritative form; the prefixed string `"tsk_…"` is the I/O-boundary
//! representation.
//!
//! [`registry`] is the entity registry: the single declaration of each entity
//! kind's fields, merge class and conflict class; the doc-schema fingerprint
//! hashes it.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod crockford;
pub mod entity_ref;
pub mod kind;
pub mod registry;
pub mod ulid;

pub use crockford::{decode_str, encode_bytes, CrockfordError};
pub use entity_ref::{EntityRef, EntityRefError};
pub use kind::EntityKind;
pub use ulid::{Ulid, UlidParseError};

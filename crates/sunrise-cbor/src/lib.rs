//! Deterministic CBOR + magic-prefix utilities + v1 version constants.
//!
//! See:
//! - `docs/10-cross-cutting/protocol-versioning.md` §3 — uniform 5-byte
//!   magic prefix `"SR" + kind:u8 + version:u16-be`.
//! - `docs/03-crypto/data-encryption-format.md` — canonical CBOR
//!   serialization rules for the op envelope.
//!
//! Canonical CBOR rules enforced here:
//! - Map keys sorted by their CBOR-encoded byte sequence (RFC 8949 §4.2.3
//!   "Core Deterministic Encoding").
//! - Shortest-form integer encoding.
//! - Definite-length items only.
//! - No floating-point in v1 envelopes (use millisecond integers instead).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod canonical;
pub mod cbor_value;
pub mod hlc;
pub mod magic;
pub mod version;

pub use canonical::{
    decode_canonical, decode_lenient, encode_canonical, CanonicalEncoding, CanonicalError,
};
pub use cbor_value::CborValue;
pub use hlc::{Hlc, HlcError, MAX_DRIFT_MS};
pub use magic::{decode_prefix, write_prefix, MagicError, MagicKind, MagicPrefix, MAGIC_LEN};
pub use version::{
    CRYPTO_SUITE_V, DOC_SCHEMA_FLOOR, DOC_SCHEMA_V, ENVELOPE_FORMAT_V, STORAGE_V, WIRE_PROTO_V,
};

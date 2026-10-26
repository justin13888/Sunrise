//! Deterministic CBOR, magic prefixes, hybrid logical clocks, the cleartext
//! envelope header, and the protocol version constants.
//!
//! Modules:
//! - [`canonical`] — canonical CBOR encode and decode.
//! - [`bounded`] — depth-limited decoding of untrusted CBOR, at
//!   [`MAX_DOC_DEPTH`] for document content and [`MAX_VALUE_DEPTH`] for
//!   framing.
//! - [`cbor_value`] — [`CborValue`], a preserved CBOR value that refuses
//!   floats on decode.
//! - [`hlc`] — the hybrid logical clock [`Hlc`] and its drift bound
//!   [`MAX_DRIFT_MS`].
//! - [`envelope_header`] — the cleartext routing header of an `OpEnvelope`,
//!   decoded without its contents.
//! - [`magic`] — the 5-byte magic prefix.
//! - [`version`] — the version constants and floors, and the
//!   [`DOC_SCHEMA_FINGERPRINTS`] registry.
//!
//! See:
//! - `docs/10-cross-cutting/protocol-versioning.md` §3 — uniform 5-byte
//!   magic prefix `"SR" + kind:u8 + version:u16-be`.
//! - `docs/03-crypto/data-encryption-format.md` — canonical CBOR
//!   serialization rules for the op envelope.
//!
//! Canonical CBOR rules:
//! - Map keys sorted by their CBOR-encoded byte sequence (RFC 8949 §4.2.3
//!   "Core Deterministic Encoding").
//! - Shortest-form integer encoding.
//! - Definite-length items only.
//! - No floating-point (use millisecond integers instead). The encoder does
//!   not check this: a float is refused where a value decodes as
//!   [`CborValue`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bounded;
pub mod canonical;
pub mod cbor_value;
pub mod envelope_header;
pub mod hlc;
pub mod magic;
pub mod version;

pub use bounded::{decode_bounded, decode_value, ValueDecodeError, MAX_DOC_DEPTH, MAX_VALUE_DEPTH};
pub use canonical::{
    decode_canonical, decode_canonical_within, decode_lenient, encode_canonical, CanonicalError,
};
pub use cbor_value::CborValue;
pub use envelope_header::{
    decode_envelope_header, envelope_floor_readable, envelope_format_readable, EnvelopeHeader,
    EnvelopeHeaderError,
};
pub use hlc::{Hlc, HlcError, MAX_DRIFT_MS};
pub use magic::{decode_prefix, write_prefix, MagicError, MagicKind, MagicPrefix, MAGIC_LEN};
pub use version::{
    doc_schema_fingerprint, doc_schema_fp_prefix, CRYPTO_SUITE_V, DOC_SCHEMA_FINGERPRINTS,
    DOC_SCHEMA_FLOOR, DOC_SCHEMA_FP_DOMAIN, DOC_SCHEMA_FP_FIRST, DOC_SCHEMA_FP_PREFIX_LEN,
    DOC_SCHEMA_V, ENVELOPE_FORMAT_FLOOR, ENVELOPE_FORMAT_V, STORAGE_V, WIRE_PROTO_V,
};

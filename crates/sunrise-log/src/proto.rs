//! Protocol versions field.
//!
//! Per `spec/10-cross-cutting/logging.md` §3 + `protocol-versioning.md`,
//! every record carries `proto: { wire, doc, crypto }`. These come from the
//! version constants in `sunrise-cbor`/`sunrise-wire-protocol`/
//! `sunrise-crypto`. The logger doesn't depend on those crates (would be a
//! cycle); instead it accepts numeric versions on init and pins them for the
//! process lifetime.

use serde::{Deserialize, Serialize};

/// Versions of the four protocol surfaces relevant to logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtoVersions {
    /// `WIRE_PROTO_V` from `sunrise-wire-protocol`.
    pub wire: u32,
    /// `DOC_SCHEMA_V` from `sunrise-cbor` / `sunrise-domain`.
    pub doc: u32,
    /// `CRYPTO_SUITE_V` from `sunrise-crypto`.
    pub crypto: u32,
}

impl ProtoVersions {
    /// All-ones placeholder used before [`crate::init`] runs; sentinel.
    pub const UNSET: Self = Self {
        wire: 0,
        doc: 0,
        crypto: 0,
    };
}

impl Default for ProtoVersions {
    fn default() -> Self {
        Self::UNSET
    }
}

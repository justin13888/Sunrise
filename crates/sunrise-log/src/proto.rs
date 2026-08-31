//! Protocol versions, logged once at boot.
//!
//! `docs/10-cross-cutting/logging.md` §3 originally put a `proto: {wire, doc,
//! crypto}` block on *every* record so cross-version logs could be reasoned
//! about. Under `tracing` that would mean a custom event formatter injecting
//! three constants into every line — roughly 50 bytes per record to restate
//! something that cannot change while the process lives.
//!
//! So the versions are recorded once, on the binary's startup event, as
//! `wire_v` / `doc_v` / `crypto_v`. Correlating a later record with them is a
//! matter of finding the `*.start` line for the same process, which any
//! ingest pipeline groups anyway. See the amendment in
//! `docs/11-adr/0010-logging-strategy.md`.
//!
//! The struct exists so both binaries report the same three numbers in the
//! same shape; it does not depend on the crates that define them (that would
//! be a cycle) and instead takes them as values.

/// Versions of the three protocol surfaces relevant to logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtoVersions {
    /// `WIRE_PROTO_V` from `sunrise-cbor`.
    pub wire: u32,
    /// `DOC_SCHEMA_V` from `sunrise-cbor` / `sunrise-domain`.
    pub doc: u32,
    /// `CRYPTO_SUITE_V` from `sunrise-cbor` / `sunrise-crypto`.
    pub crypto: u32,
}

impl ProtoVersions {
    /// All-zero sentinel, meaning "not reported".
    pub const UNSET: Self = Self {
        wire: 0,
        doc: 0,
        crypto: 0,
    };

    /// Construct from the three version constants.
    #[must_use]
    pub const fn new(wire: u16, doc: u16, crypto: u16) -> Self {
        Self {
            wire: wire as u32,
            doc: doc as u32,
            crypto: crypto as u32,
        }
    }
}

impl Default for ProtoVersions {
    fn default() -> Self {
        Self::UNSET
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_the_unset_sentinel() {
        assert_eq!(ProtoVersions::default(), ProtoVersions::UNSET);
        assert_eq!(ProtoVersions::UNSET.wire, 0);
    }

    #[test]
    fn new_widens_the_u16_constants() {
        let p = ProtoVersions::new(1, 2, 3);
        assert_eq!((p.wire, p.doc, p.crypto), (1, 2, 3));
    }
}

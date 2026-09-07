//! Hello / HelloAck session negotiation.
//!
//! Per `docs/10-cross-cutting/protocol-versioning.md` §4.

use crate::capability::{CapabilityBits, REQUIRED_CLIENT_BITS, REQUIRED_SERVER_BITS};
use serde::{Deserialize, Serialize};
use sunrise_error::ErrorCode;
use thiserror::Error;

/// Client → Server greeting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hello {
    /// Client app version (e.g., `"1.4.2"`).
    pub client_app_v: String,
    /// Platform string (e.g., `"linux-x86_64"`).
    pub client_platform: String,
    /// Ascending list of `WIRE_PROTO_V` values the client speaks.
    pub wire_proto_supported: Vec<u32>,
    /// Lowest `DOC_SCHEMA_V` the client can produce.
    pub doc_schema_min: u32,
    /// Highest `DOC_SCHEMA_V` the client can read.
    pub doc_schema_max: u32,
    /// Crypto suites the client speaks.
    pub crypto_suite_supported: Vec<u32>,
    /// Bitfield of optional features.
    pub capabilities: u64,
    /// ULID of the session trace.
    pub trace: String,
}

/// Server → Client confirmation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HelloAck {
    /// Server app version.
    pub server_app_v: String,
    /// Negotiated wire protocol version.
    pub wire_proto: u32,
    /// Negotiated crypto suite.
    pub crypto_suite: u32,
    /// Server's accepted `doc_schema_min` (its floor).
    pub doc_schema_floor: u32,
    /// AND'd capability bitfield.
    pub capabilities: u64,
    /// Server time (advisory; for clock-skew detection).
    pub server_time_ms: u64,
}

/// Negotiation outcome errors.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum NegotiationError {
    /// Wire-protocol intersection empty.
    #[error("wire-protocol versions don't intersect")]
    WireMismatch,
    /// Crypto-suite intersection empty.
    #[error("crypto suite versions don't intersect")]
    CryptoMismatch,
    /// Server's doc_schema_floor exceeds client's max.
    #[error("doc_schema_floor too high")]
    DocSchemaTooOld,
    /// Required capability bit missing.
    #[error("required capability missing")]
    CapabilityRequiredMissing,
}

impl NegotiationError {
    /// Map to a canonical wire error code.
    ///
    /// This is what the client actually receives:
    /// `crates/sunrise-server/src/api/sync.rs`'s `session` handler puts the
    /// returned code on the `400` rather than collapsing every refusal to one
    /// generic code. The four are four different outcomes for whoever is
    /// looking at the screen — update the app, update the relay, this device's
    /// data predates the relay's floor, this relay lacks a feature the vault
    /// requires — so anything that stops calling this pushes a client author
    /// into matching on `Display` output to recover the distinction.
    #[must_use]
    pub const fn as_error_code(&self) -> ErrorCode {
        match self {
            Self::WireMismatch => ErrorCode::SyncProtocolVersionMismatch,
            Self::CryptoMismatch => ErrorCode::CryptoSuiteMismatch,
            Self::DocSchemaTooOld => ErrorCode::DocSchemaTooOld,
            Self::CapabilityRequiredMissing => ErrorCode::CapabilityRequiredMissing,
        }
    }
}

impl Hello {
    /// Compute the negotiated values against the server's profile.
    ///
    /// Returns the `HelloAck` body to send back, OR a [`NegotiationError`]
    /// the server should surface as an `Error` frame and close.
    ///
    /// `server_wire_supported` and `server_crypto_supported` are the server's
    /// supported version sets; `server_doc_floor` is the server's lowest
    /// accepted doc-schema; `server_capabilities` is the server's bitfield.
    /// `server_time_ms` is the server's wall-clock at negotiation.
    pub fn negotiate(
        &self,
        server_app_v: String,
        server_wire_supported: &[u32],
        server_crypto_supported: &[u32],
        server_doc_floor: u32,
        server_capabilities: u64,
        server_time_ms: u64,
    ) -> Result<HelloAck, NegotiationError> {
        let wire = max_intersection(&self.wire_proto_supported, server_wire_supported)
            .ok_or(NegotiationError::WireMismatch)?;
        let crypto = max_intersection(&self.crypto_suite_supported, server_crypto_supported)
            .ok_or(NegotiationError::CryptoMismatch)?;
        if server_doc_floor > self.doc_schema_max {
            return Err(NegotiationError::DocSchemaTooOld);
        }
        // Verify required capabilities.
        let client_bits = CapabilityBits(self.capabilities);
        if (client_bits.0 & REQUIRED_CLIENT_BITS.0) != REQUIRED_CLIENT_BITS.0 {
            return Err(NegotiationError::CapabilityRequiredMissing);
        }
        let server_bits = CapabilityBits(server_capabilities);
        if (server_bits.0 & REQUIRED_SERVER_BITS.0) != REQUIRED_SERVER_BITS.0 {
            return Err(NegotiationError::CapabilityRequiredMissing);
        }
        let agreed = client_bits.intersection(server_bits);
        Ok(HelloAck {
            server_app_v,
            wire_proto: wire,
            crypto_suite: crypto,
            doc_schema_floor: server_doc_floor,
            capabilities: agreed.0,
            server_time_ms,
        })
    }
}

fn max_intersection(client: &[u32], server: &[u32]) -> Option<u32> {
    client.iter().filter(|c| server.contains(c)).copied().max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capability;

    /// Each side advertises the union of bits IT supports — including bits
    /// owned by the *other* side if it is willing to honor them. A v1 client
    /// sets its CLI_* bits AND the SRV_* bits it expects the server to
    /// provide; a v1 server sets its SRV_* bits AND the CLI_* bits it
    /// understands. The AND of the two gives the agreed feature set.
    fn fixture_hello() -> Hello {
        Hello {
            client_app_v: "1.4.2".into(),
            client_platform: "linux-x86_64".into(),
            wire_proto_supported: vec![1],
            doc_schema_min: 1,
            doc_schema_max: 1,
            crypto_suite_supported: vec![1],
            capabilities: REQUIRED_CLIENT_BITS.0 | REQUIRED_SERVER_BITS.0,
            trace: "01HXYZ".into(),
        }
    }

    fn server_caps() -> u64 {
        REQUIRED_SERVER_BITS.0 | REQUIRED_CLIENT_BITS.0
    }

    #[test]
    fn negotiates_v1() {
        let hello = fixture_hello();
        let ack = hello
            .negotiate("0.1.0".into(), &[1], &[1], 1, server_caps(), 0)
            .unwrap();
        assert_eq!(ack.wire_proto, 1);
        assert_eq!(ack.crypto_suite, 1);
        assert_eq!(ack.doc_schema_floor, 1);
        assert!(CapabilityBits(ack.capabilities).has(Capability::CliEntityLww));
        assert!(CapabilityBits(ack.capabilities).has(Capability::SrvBlobPresign));
    }

    #[test]
    fn rejects_no_wire_intersection() {
        let mut hello = fixture_hello();
        hello.wire_proto_supported = vec![2];
        let err = hello
            .negotiate("0".into(), &[1], &[1], 1, server_caps(), 0)
            .unwrap_err();
        assert_eq!(err, NegotiationError::WireMismatch);
    }

    #[test]
    fn rejects_doc_schema_too_old() {
        let hello = fixture_hello();
        let err = hello
            .negotiate("0".into(), &[1], &[1], 99, server_caps(), 0)
            .unwrap_err();
        assert_eq!(err, NegotiationError::DocSchemaTooOld);
    }

    #[test]
    fn rejects_missing_required_client_bit() {
        let mut hello = fixture_hello();
        hello.capabilities = 0; // missing all required client bits
        let err = hello
            .negotiate("0".into(), &[1], &[1], 1, server_caps(), 0)
            .unwrap_err();
        assert_eq!(err, NegotiationError::CapabilityRequiredMissing);
    }
}

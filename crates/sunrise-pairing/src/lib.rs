//! Noise XX pairing handshake + QR payload + SAS code computation.
//!
//! Per `docs/03-crypto/pairing-and-onboarding.md`. v1 implements:
//!
//! - QR payload encode/decode (lex-ordered JSON UTF-8, base64url-no-pad).
//! - Short Authentication String (SAS) — 6-digit decimal derived from the
//!   Noise handshake hash via `BLAKE3("sunrise.pair_sas.v1" || h, 3)`.
//! - Rate limit constants (10 attempts / hour, 30 / day per
//!   `account_email_hash`).
//!
//! - The full Noise XX transcript ([`handshake`]), including the encrypted
//!   channel the existing device uses to hand the new device its vault root.
//!
//! Until this crate was wired up there was no way to get a vault root onto a
//! second device: sync worked in tests only because both replicas were handed
//! the same literal key. Encryption was real; key distribution was bypassed.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::doc_markdown, clippy::missing_errors_doc)]

pub mod handshake;
pub mod qr;
pub mod rate_limit;
pub mod sas;

pub use handshake::{
    PairedChannel, PairingError, PairingSession, Role, MAX_NOISE_MESSAGE, NOISE_PARAMS,
};
pub use qr::{decode_qr_payload, encode_qr_payload, QrPayload, QrPayloadError, MAGIC_V1_HEX};
pub use rate_limit::{account_email_hash, RATE_LIMIT_DAILY, RATE_LIMIT_HOURLY};
pub use sas::{compute_sas, SAS_LEN};

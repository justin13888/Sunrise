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
//! The full Noise XX transcript wires through `snow` as part of the
//! transport-driven session; this crate exposes the building blocks and
//! the SAS so callers can confirm the handshake.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::doc_markdown, clippy::missing_errors_doc)]

pub mod qr;
pub mod rate_limit;
pub mod sas;

pub use qr::{decode_qr_payload, encode_qr_payload, QrPayload, QrPayloadError, MAGIC_V1_HEX};
pub use rate_limit::{account_email_hash, RATE_LIMIT_DAILY, RATE_LIMIT_HOURLY};
pub use sas::{compute_sas, SAS_LEN};

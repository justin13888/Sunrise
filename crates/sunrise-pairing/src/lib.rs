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
//!
//! # Pairing is three messages, not one
//!
//! [`protocol`] is the exchange itself and is transport-agnostic on purpose:
//!
//! 1. [`PairingOffer`] — the sponsor's account identity, **public halves only**.
//! 2. [`PairingRequest`] — the joiner's freshly minted `D_S_pub` / `D_D_pub`.
//! 3. [`PairingGrant`] — the `DeviceCert` the sponsor issued for those keys,
//!    plus the vault root and every Stream key.
//!
//! The joiner assembles the three into a [`PairingPayload`] ([`payload`]) and
//! hands that to `Keychain::create`. Nothing in any of the four carries
//! `ID_S_priv`, which is the point: a `DeviceCert` names only its subject and is
//! signed only by the identity, so any device holding `ID_S_priv` can mint a
//! valid cert for any device id it invents — which is how a revoked device used
//! to rejoin ([#105](https://github.com/justin13888/Sunrise/issues/105)). The
//! round trip exists because the sponsor cannot sign a cert for keys the joiner
//! has not minted yet.
//!
//! Whether the three map onto Noise transport frames (`sunrise-core-bindings`,
//! and so the Apple clients) or onto files (`sunrise-cli`) is the caller's
//! business. Both get the same bytes and the same checks.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(clippy::doc_markdown, clippy::missing_errors_doc)]

pub mod handshake;
pub mod payload;
pub mod protocol;
pub mod qr;
pub mod rate_limit;
pub mod sas;

pub use handshake::{
    PairedChannel, PairingError, PairingSession, Role, StaticKeyPair, MAX_NOISE_MESSAGE,
    NOISE_PARAMS,
};
pub use payload::{
    decode_pairing_payload, encode_pairing_payload, PairingPayload, PairingPayloadError,
    MAX_PAIRING_PAYLOAD,
};
pub use protocol::{
    decode_pairing_grant, decode_pairing_offer, decode_pairing_request, PairingGrant, PairingOffer,
    PairingRequest,
};
pub use qr::{decode_qr_payload, encode_qr_payload, QrPayload, QrPayloadError, MAGIC_V1_HEX};
pub use rate_limit::{account_email_hash, RATE_LIMIT_DAILY, RATE_LIMIT_HOURLY};
pub use sas::{compute_sas, SAS_LEN};

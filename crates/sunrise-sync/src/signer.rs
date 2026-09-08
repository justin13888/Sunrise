//! What a transport needs in order to bind a request to the device sending it.
//!
//! ADR-0022's `header_sig_v2` puts three headers on every authenticated
//! request: `X-Sunrise-Device` naming which of the account's devices is
//! calling, `Date`, and `X-Sunrise-Device-Sig` over both plus the method, the
//! target and the canonical body. Until this existed the scheme was implemented
//! once, on the verifying side: `sunrise-server` checked a signature no client
//! in the workspace produced, and with `require_device_sig` on every signed
//! route answered 401 — including the revocation `DELETE`, whose whole purpose
//! is to work when a device has been lost.
//!
//! # Why a trait, and why this shape
//!
//! The signing key is `D_S_priv`, which lives wrapped under the vault root
//! inside `sunrise_core::Keychain` and is deliberately not gettable: the one
//! accessor that hands key material out is named
//! `export_vault_root_for_pairing`, at that length, precisely so that adding a
//! second is a decision somebody has to defend. A transport does not need the
//! key — it needs signatures — so this asks for the *operation* rather than the
//! secret, and the core answers with 64 bytes while `D_S_priv` stays where it
//! is.
//!
//! [`DeviceSigner::now_ms`] is here for the same reason the key is not: the
//! `Date` header is signed, and a transport that read `SystemTime::now` would
//! be reading an ambient clock, which `clippy.toml`'s disallowed-methods list
//! and the CI determinism gate both forbid. The implementor already holds an
//! injected clock, so the time comes from there — which is also what makes a
//! skewed client testable rather than a thing that only happens in the field.
//!
//! It lives outside the `sse` feature because `sunrise-core` implements it and
//! does not enable that feature; the transport that consumes it is behind it.

/// The device binding a transport presents on every authenticated request.
///
/// One implementor in production — `sunrise_core::Core::device_signer` — and
/// whatever a test needs.
pub trait DeviceSigner: Send + Sync + std::fmt::Debug {
    /// The relay's id for this device: the `X-Sunrise-Device` value.
    ///
    /// A ULID the relay mints at registration and returns from
    /// `POST /api/v1/devices`. Not the vault's own 16-byte device id, which the
    /// relay has never held for any device but the one registering.
    fn device_id(&self) -> String;

    /// Sign `message` with `D_S_priv`; 64 raw Ed25519 bytes.
    ///
    /// `message` is `sunrise_http_sig::canonical_string`'s output. Nothing
    /// about the scheme is decided here — the caller builds the canonical
    /// string and encodes the result — so an implementor cannot get the wire
    /// form wrong.
    fn sign(&self, message: &[u8]) -> [u8; 64];

    /// Unix milliseconds, from the implementor's injected clock.
    fn now_ms(&self) -> u64;
}

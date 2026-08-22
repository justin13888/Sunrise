//! v1 protocol-version constants.
//!
//! Per `docs/10-cross-cutting/protocol-versioning.md` §2. The four versioned
//! surfaces are exposed as `u16` because the magic-prefix layout encodes
//! them in 2 bytes big-endian; the wire protocol's `Hello` / `HelloAck`
//! frames also carry them as small unsigned ints.

/// Wire protocol version constant (frame layout, message kinds, error codes).
pub const WIRE_PROTO_V: u16 = 1;

/// Document schema version constant (per-entity field shapes, CRDT types).
pub const DOC_SCHEMA_V: u16 = 1;

/// Crypto suite version constant (AEAD, signature, KDF, HPKE choices).
pub const CRYPTO_SUITE_V: u16 = 1;

/// Local storage schema version. Per-device; never appears on the wire.
pub const STORAGE_V: u16 = 9;

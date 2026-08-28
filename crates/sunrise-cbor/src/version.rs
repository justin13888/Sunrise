//! v1 protocol-version constants.
//!
//! Per `docs/10-cross-cutting/protocol-versioning.md` §2. The versioned
//! surfaces are exposed as `u16` because the magic-prefix layout encodes
//! them in 2 bytes big-endian; the wire protocol's `Hello` / `HelloAck`
//! frames also carry them as small unsigned ints.

/// Wire protocol version constant (frame layout, message kinds, error codes).
pub const WIRE_PROTO_V: u16 = 1;

/// Envelope **container format** version: the `OpEnvelope` field layout, its
/// canonical-CBOR ordering, the AAD construction, and the signature input.
///
/// This is deliberately *not* [`DOC_SCHEMA_V`]. The container and the document
/// schema it carries evolve independently: adding a field to a Task must not
/// make every already-signed envelope undecodable. A reader rejects an envelope
/// whose container format it does not implement, because it cannot locate the
/// bytes; it accepts any document schema at or above its floor, because the
/// container tells it where the payload is regardless.
///
/// `2` is the first format that carries the document schema as its own field
/// (field 12). See ADR-0015.
pub const ENVELOPE_FORMAT_V: u16 = 2;

/// Document schema version constant (per-entity field shapes).
///
/// Rides in `OpEnvelope` field 12 and in `Hello.doc_schema_{min,max}`. Bumping
/// it is a **forward-compatible** act: older readers keep decoding, and see
/// fields they do not know as preserved unknowns.
pub const DOC_SCHEMA_V: u16 = 1;

/// Lowest [`DOC_SCHEMA_V`] this build can still interpret.
///
/// An envelope below the floor is refused: its entity shapes predate anything
/// this build knows how to read. An envelope at or above it is accepted —
/// forward compatibility is the whole point of splitting the two versions.
pub const DOC_SCHEMA_FLOOR: u16 = 1;

/// Crypto suite version constant (AEAD, signature, KDF, HPKE choices).
pub const CRYPTO_SUITE_V: u16 = 1;

/// Local storage schema version. Per-device; never appears on the wire.
pub const STORAGE_V: u16 = 12;

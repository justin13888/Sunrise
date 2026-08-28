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
/// `2` was the first format to carry the document schema as its own field
/// (field 12); `3` changed field 5 from a bare wall-clock millisecond count to
/// a hybrid logical clock `[physical_ms, logical]`. Neither number ever
/// shipped: v1 opens at `3`. See ADR-0015 and ADR-0016.
pub const ENVELOPE_FORMAT_V: u16 = 3;

/// Document schema version constant (per-entity field shapes).
///
/// Rides in `OpEnvelope` field 12 and in `Hello.doc_schema_{min,max}`. Bumping
/// it is a **forward-compatible** act: older readers keep decoding, and see
/// fields they do not know as preserved unknowns.
///
/// `2` re-typed `Task.scheduled_at` / `due_at` / `completed_at` and
/// `Block.starts_at` / `ends_at` from a bare instant to a tagged
/// `SunriseTime` (issue #6, ADR-0017). A v1 payload still decodes: the bare
/// instant reads as `SunriseTime::Instant`, which is why the floor stays at 1.
///
/// `3` added `Block.title_track_task`, `Task.reminder_lead_s` and
/// `Stream.reminder_lead_s`, and the `blk_` / `att_` op families (issues #22,
/// #9). Every one of those is an *addition*: a v2 payload decodes here with
/// the new fields at their defaults, and a v2 reader keeps a v3 payload's
/// unknown fields verbatim through the `unknown` map. The floor therefore
/// stays at 1.
///
/// A v2 build handed a `blk_` or `att_` op cannot decode it — a new op variant
/// is not a new field — and reports it as an invalid remote op rather than
/// applying it wrongly. That is acceptable pre-1.0, where no build older than
/// this one exists (ADR-0018), and it is why the op vocabulary is documented
/// as a wire contract in `sunrise_core::inner_op`.
pub const DOC_SCHEMA_V: u16 = 4;

/// Lowest [`DOC_SCHEMA_V`] this build can still interpret.
///
/// An envelope below the floor is refused: its entity shapes predate anything
/// this build knows how to read. An envelope at or above it is accepted —
/// forward compatibility is the whole point of splitting the two versions.
///
/// Still `1` even though [`DOC_SCHEMA_V`] is `2`, because a v1 payload really
/// does still decode. The floor moves only when a shape stops being readable,
/// never merely because a newer one exists.
pub const DOC_SCHEMA_FLOOR: u16 = 1;

/// Crypto suite version constant (AEAD, signature, KDF, HPKE choices).
pub const CRYPTO_SUITE_V: u16 = 1;

/// Local storage schema version. Per-device; never appears on the wire.
pub const STORAGE_V: u16 = 13;

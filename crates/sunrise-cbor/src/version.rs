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
///
/// `5` added the three control op families — `KeyEnvelope`, `DeviceRevoke` and
/// `DeviceCertPublish` (ADR-0024). Like `blk_` and `att_` before them these are
/// new *variants*, so an older build refuses one rather than misreading it, and
/// the floor still does not move: every v1..v4 payload shape is unchanged and
/// still decodes here.
pub const DOC_SCHEMA_V: u16 = 5;

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
///
/// `2` is ADR-0024: Stream keys stopped being derived from the vault root and
/// became independently random per `(stream_id, epoch)`, distributed by RFC
/// 9180 HPKE Base envelopes (DHKEM(X25519,HKDF-SHA256) / HKDF-SHA256 /
/// ChaCha20-Poly1305), and device certs became identity-signed rather than
/// self-signed. None of the *primitives* changed — which is why
/// [`ENVELOPE_FORMAT_V`] does not move — but the key schedule they are applied
/// to did, and that is what this constant names.
pub const CRYPTO_SUITE_V: u16 = 2;

/// Local storage schema version. Per-device; never appears on the wire.
///
/// `17` was migration `0017_key_hierarchy.sql`: the `identity` table, the
/// re-keyed `stream_keys`, `deferred_ops`, and the device/revocation columns
/// ADR-0024 needs.
///
/// `18` is migration `0018_key_envelope_recipients.sql`, which adds the index
/// of which device has been sent which `(stream, epoch)` key. That question was
/// unanswerable from the database before — the recipient lives inside the
/// `key_envelope` op's sealed payload — and nothing needed to ask it while
/// every device held `ID_D_priv` and could open the identity copy of any
/// envelope it was left out of. Removing `ID_D_priv` from `PairingPayload`
/// removes that fallback, so `Engine::backfill_key_envelopes` has to know what
/// is actually missing, and this table is what it reads.
///
/// `19` is migration `0019_identity_minted_by.sql`. 0018 left one hole open in
/// terms: a device paired while this constant read `17` had wrapped the
/// account's `ID_D_priv` into its own `identity` row, kept it through the
/// upgrade, and went on opening the identity-sealed copy of every rotated
/// epoch. Clearing the column needed a way to tell that device from the
/// account's *creator*, on which the same column holds the only copy of the
/// key. `stream_keys.source` already carried the answer -- only the creator
/// mints the vault-meta stream's first epoch -- so 19 reads it, records the
/// minting device on the `identity` row, and clears the column everywhere
/// else.
///
/// `20` is migration `0020_relay_revocation_intents.sql`, the queue that
/// carries the relay's half of a revocation. A `device_revoke` op is sealed
/// under the vault-meta Stream key and the relay holds no Stream keys, so the
/// relay has to be told out of band; `revoke_device` must work with no network,
/// because a device that is gone is the whole scenario, so the telling cannot
/// be part of the command. The row is what remembers it is owed.
///
/// `21` is migration `0021_ops_by_ts.sql`, an index on `ops (ts_ms)`. The HLC
/// restore at open (`Engine::prime_hlc`, ADR-0036) reads the greatest stamp in
/// the op log, and `ops` carried no index on that column, so every open ran two
/// full scans of the widest table in the vault, decrypting it a page at a time.
/// Measured at 18.1 ms for a 10k-op log, 183 ms at 100k and 2.01 s at 1M;
/// with the index, about 60 us at all three.
pub const STORAGE_V: u16 = 21;

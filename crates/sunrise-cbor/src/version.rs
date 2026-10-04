//! Protocol-version constants (wire protocol 1 and its sibling surfaces).
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
///
/// A writer stamps this in envelope field 1, and stamps
/// [`ENVELOPE_FORMAT_FLOOR`] in the magic prefix (ADR-0045 §5). A reader takes
/// any container whose floor it implements, so an *additive* change — a new
/// field whose absence has a defined meaning — bumps this constant alone.
///
/// Every build before the floor compares both numbers for equality, so this
/// MUST stay equal to [`ENVELOPE_FORMAT_FLOOR`] until `core.envelope_floor` is
/// in `vault_requires` and the relay agrees server capability bit 9
/// (`SrvEnvelopeFloor`). A test in `sunrise-crypto` pins that.
pub const ENVELOPE_FORMAT_V: u16 = 3;

/// Lowest envelope **container format** that still reads this build's
/// envelopes correctly, and the lowest one this build reads (ADR-0045 §5).
///
/// It rides in the envelope's magic prefix. A reader accepts an envelope when
/// `ENVELOPE_FORMAT_FLOOR <= prefix.version <= ENVELOPE_FORMAT_V` and field 1
/// is at least `prefix.version`; the rule is
/// [`crate::envelope_header::envelope_format_readable`], which the client
/// decoder and the relay's header decoder both call.
///
/// The floor MUST be raised by any container change that alters the meaning of
/// fields 1–12, the AAD construction (the `Omit` set), or the signature input
/// (`SIG_DOMAIN`). `sunrise-crypto` pins all three to the floor they were
/// frozen at, so changing one without moving the floor fails the build.
pub const ENVELOPE_FORMAT_FLOOR: u16 = 3;

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
/// applying it wrongly. That is acceptable while no build older than
/// this one exists (ADR-0018), and it is why the op vocabulary is documented
/// as a wire contract in `sunrise_core::inner_op`.
///
/// `5` added the three control op families — `KeyEnvelope`, `DeviceRevoke` and
/// `DeviceCertPublish` (ADR-0024). Like `blk_` and `att_` before them these are
/// new *variants*, so an older build refuses one rather than misreading it, and
/// the floor still does not move: every v1..v4 payload shape is unchanged and
/// still decodes here.
///
/// `6` added the fourth control family, `IdentityTransition` (ADR-0032,
/// issue #105): the account's `ID_S`/`ID_D` pair becomes replaceable, and the
/// op carries the successor's public halves, a re-issued `DeviceCert` for
/// every surviving device, and the successor's secrets sealed to each of them.
/// A new variant again, so a v5 build refuses one rather than misreading it —
/// and refusing is the right answer here rather than merely the safe one: a
/// build that skipped a transition would go on verifying every later op
/// against an identity the account has retired. Every v1..v5 payload shape is
/// unchanged, so the floor still does not move.
///
/// Those refusals describe the builds that shipped them. A build with
/// `STORAGE_V` 31 or later no longer refuses a variant it does not know: it
/// parks the verified op in `ops` and `parked_ops`, counts it toward the sync
/// cursor, and replays it through the full apply path once a build with a
/// different `DOC_SCHEMA_V` opens the vault (issue #320, ADR-0045 §4). That
/// replay is keyed on this constant, so a new variant MUST move it, or a
/// parked op of that kind is not retried until something else does.
///
/// `7` is the first version with a fingerprint (issue #323, ADR-0045 §2–§3):
/// [`DOC_SCHEMA_FP_FIRST`]. Its shapes are the ones v6 had plus what landed
/// since without a bump, all of it additive: the `Unknown` arm of every
/// lossless enum (#321), unknown maps on every nested record and
/// `SunriseTime`'s unknown kind (#322). A writer at 7 stamps envelope field
/// 13 with the first 8 bytes of the fingerprint registered here, and every
/// later version MUST be registered in [`DOC_SCHEMA_FINGERPRINTS`] in the
/// change that bumps it. A test in `sunrise-core` fails while the generated
/// schema's fingerprint differs from this version's entry, so a shape cannot
/// change without the bump.
pub const DOC_SCHEMA_V: u16 = 7;

/// The first [`DOC_SCHEMA_V`] that has a fingerprint (ADR-0045 §3, `N_fp`).
///
/// An envelope below it carries no field 13 and is read under the legacy
/// rules. A writer at or above it MUST emit field 13.
pub const DOC_SCHEMA_FP_FIRST: u16 = 7;

// The build's own version is fingerprinted, so its writer can stamp field 13.
const _: () = assert!(DOC_SCHEMA_V >= DOC_SCHEMA_FP_FIRST);

/// BLAKE3 `derive_key` context for the document-schema fingerprint
/// (ADR-0045 §2): `fp = BLAKE3::derive_key(this, JCS(schema))`.
pub const DOC_SCHEMA_FP_DOMAIN: &str = "sunrise.doc_schema.fingerprint.v1";

/// How many leading bytes of the fingerprint envelope field 13 carries.
pub const DOC_SCHEMA_FP_PREFIX_LEN: usize = 8;

/// Every document-schema version that has a fingerprint, to that fingerprint,
/// in version order. **Append-only.**
///
/// This is the build's registry: the writer stamps envelope field 13 from it,
/// and a receiver compares field 13 against it. It is committed as
/// `schemas/doc-schema/registry.json`, and each entry is frozen as a literal
/// in `sunrise-crypto-test-vectors` (`protocol::DOC_SCHEMA_REGISTRY`), so an
/// entry that is edited or removed fails a test. An entry is what every build
/// that shipped it believes its version means; changing one makes two builds
/// disagree while their version numbers say they agree, which is the failure
/// the fingerprint exists to catch.
pub const DOC_SCHEMA_FINGERPRINTS: &[(u16, [u8; 32])] = &[(
    7,
    hex32("fb893b62bb2f9bf7d9adf7ba95d5bee20498a63aa0e03c4f14a7a28a7d0d6fcb"),
)];

/// The registered fingerprint of document schema `v`, or `None` for a version
/// this build has no entry for: one before [`DOC_SCHEMA_FP_FIRST`], or one
/// newer than this build.
#[must_use]
pub fn doc_schema_fingerprint(v: u32) -> Option<[u8; 32]> {
    DOC_SCHEMA_FINGERPRINTS
        .iter()
        .find(|(known, _)| u32::from(*known) == v)
        .map(|(_, fp)| *fp)
}

/// What envelope field 13 carries for document schema `v`: the first
/// [`DOC_SCHEMA_FP_PREFIX_LEN`] bytes of its registered fingerprint, or `None`
/// where [`doc_schema_fingerprint`] has no entry.
#[must_use]
pub fn doc_schema_fp_prefix(v: u32) -> Option<[u8; DOC_SCHEMA_FP_PREFIX_LEN]> {
    doc_schema_fingerprint(v).map(|fp| {
        let mut prefix = [0u8; DOC_SCHEMA_FP_PREFIX_LEN];
        prefix.copy_from_slice(&fp[..DOC_SCHEMA_FP_PREFIX_LEN]);
        prefix
    })
}

/// Decode 64 lowercase hex digits at compile time. A wrong length or digit in
/// a registry entry is a compile error.
const fn hex32(s: &str) -> [u8; 32] {
    const fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("registry fingerprints are lowercase hex"),
        }
    }
    let b = s.as_bytes();
    assert!(b.len() == 64, "a fingerprint is 64 hex digits");
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = (nibble(b[2 * i]) << 4) | nibble(b[2 * i + 1]);
        i += 1;
    }
    out
}

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
///
/// `3` re-derives the blob-chunk nonce. It was
/// `BLAKE3.derive_key("sunrise.blob_chunk_nonce.v1", blob_key ||
/// u32_be(chunk_idx))`, with `chunk_count` bound in the chunk's AAD and
/// nowhere else — and AAD does not enter the keystream. Two different
/// chunkings of one plaintext under one `blob_key` therefore shared a
/// keystream. The derivation now hashes the chunk AAD itself
/// (`"sunrise.blob_chunk_nonce.v2"`, `blob_key || chunk_aad`), so everything
/// the AAD distinguishes the nonce distinguishes too, by construction rather
/// than by two definitions being kept in step. No primitive changed, which is
/// again why [`ENVELOPE_FORMAT_V`] does not move: the key schedule did.
///
/// Nothing is deployed, so no blob sealed under `2` exists to migrate. A `2`
/// chunk is simply unopenable here, and that is the intended behaviour of a
/// suite bump.
///
/// `4` splits the AAD of the account identity's two wrapped halves. Both
/// `ID_S_priv` and `ID_D_priv` were sealed under
/// `"sunrise.local_identity.identity.v1" || identity_id`, so either blob opened
/// under the other's domain and the `identity` row's two columns were
/// interchangeable to the AEAD. They are now
/// `"sunrise.local_identity.identity.sign.v2"` and
/// `"sunrise.local_identity.identity.dh.v2"`.
///
/// [`STORAGE_V`] deliberately does **not** move for it. The change is to the
/// wrapping domain of two blobs and to no table shape, and `STORAGE_V` is
/// defined as the id of the last migration
/// (`sunrise_storage::migrations::current_storage_v`, asserted equal to this
/// constant), so bumping it would mean writing a migration with no DDL in it.
/// A vault written under the shared domain simply does not open here, which is
/// what a suite bump is supposed to do and what the absence of any deployment
/// makes free.
///
/// `5` changes how a `DeviceCert` carries its body. It was a nested CBOR map,
/// so a verifier parsed it and re-encoded the parse to rebuild the signature
/// input — checking the signature against a *re-encoding* rather than against
/// the bytes that arrived. Every parse/encode asymmetry was therefore a
/// verification gap. The body now travels as an opaque `bstr` and the signature
/// covers exactly those bytes, which is the construction COSE uses for its
/// protected header. This is a change to how a signature input is built, which
/// is what this constant names; the signature algorithm is unchanged.
pub const CRYPTO_SUITE_V: u16 = 5;

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
///
/// `22` is migration `0022_identity_transition.sql`. The account identity
/// stopped being a value and became a chain: `identity_transitions` holds one
/// append-only row per absorbed transition, and `identity.genesis_identity_id`
/// is the fixed point the chain is folded from. `identity.identity_id` could
/// not serve as that anchor — it is the identity *in force*, so it moves on
/// every transition, and replicas at different points in the chain would fold
/// from different starts and disagree about who the account is.
///
/// `23` is migration `0023_identity_chain_verification.sql`. 22 built the chain
/// and left it unverifiable: the fold needs `(identity_id, ID_S_pub)` per link
/// to check each transition's `prev_sig`, and the *genesis* key is in no row
/// once `identity.id_s_pub` moves to the successor -- `identity_id` is a
/// one-way derivation of it. It also needs the two digests the signatures are
/// taken over, which were inside the payload blob, so the fold would have had
/// to CBOR-decode a roster on every link at every open.
///
/// `24` is migration `0024_attachment_upload.sql`, which makes an attachment's
/// bytes reachable from a second device (issue #176). It adds
/// `attachments.ciphertext_hash` — the only thing that names a blob on the
/// relay, since `finalize` content-addresses by the ciphertext and
/// `content_hash` covers the plaintext — and `blob_uploads`, the durable queue
/// of blobs whose chunks are sealed locally and not yet committed upstream.
/// The queue is a table rather than a direct call for the reason 0020's is:
/// attaching a file has to work offline.
///
/// `25` is migration `0025_blob_fetch_requests.sql`, the other half of that
/// route (issue #227). 24 made a blob *reachable*; the 10 MiB auto-fetch
/// threshold in `docs/02-domain/attachments.md` §Lazy fetch then decided which
/// blobs were actually reached, and nothing carried the rest — an attachment
/// over the threshold was unreachable on every device but the one that sealed
/// it. `blob_fetches` is the durable record of "the user pressed Download on
/// this one", read by the sync driver, which holds the transport, and it
/// carries the document's `partial` cache state for a transfer the user
/// cancelled.
/// `26` is migration `0026_device_admitted_after_revocation.sql`, which keeps a
/// signal the core had and only logged (issue #144). `DeviceCertPublish`
/// already recognises a device id this vault has never seen arriving in an
/// account that has revoked something, and emitted
/// `core.device.admitted_after_revocation` about it; the column is the same
/// answer written down, because the predicate is about the moment the cert
/// applied and nothing else in the schema can reconstruct that afterwards. It
/// feeds the device list on every client rather than an operator's NDJSON.
///
/// `27` is migration `0027_device_revoke_ops.sql`, the ledger that turns
/// `device_revocations` from a running upsert into a fold (issue #82,
/// ADR-0041). Keeping every `device_revoke` op is what lets a replica skip one
/// whose sender the account had already revoked without the answer depending on
/// which of the two ops it saw first, and what lets a skipped op be folded again
/// when somebody revokes that sender's revoker and the gate stops reading the
/// sender as revoked. A cut correction is not that: the gate reads no cut.
///
/// `28` is migration `0028_device_read_bounds.sql`, which separates the read
/// bound from the register 27 made derived. The register answers "is this
/// device currently called revoked?", which has to converge and therefore has
/// to be a fold that can take a row back out; the four key-distribution sites
/// ask "is this device read-bounded?", which has to be monotone or it is not a
/// bound. One table could not be both, so the second question gets its own
/// ratchet — never deleted for a device this replica holds a cert for — and an
/// unwound revocation stops handing the device back every epoch the vault
/// mints (ADR-0041 §Decision 1 records the unwind; this is its read half).
///
/// `29` is migration `0029_stream_key_senders.sql`, which records which devices
/// delivered each Stream key this vault absorbed (issue #280, ADR-0041
/// §Decision 4). A key a read-bounded device delivered is still stored, so
/// every op sealed under it stays readable, and it is never the key this
/// device writes under.
///
/// `30` is migration `0030_read_bounds_from_sponsor.sql`, which marks the read
/// bounds a paired device adopted from its sponsor's pairing grant (issue
/// #282). A device that paired used to start with no bound and learn the
/// revocations from the relay in whatever order it had them, which left it the
/// weakest replica in the account; it now starts with its sponsor's bound, and
/// the mark keeps the orphan release from taking an adopted bound back before
/// the device's cert arrives.
///
/// `31` is migration `0031_parked_ops.sql`, which parks a verified op whose
/// inner kind this build does not know instead of dropping it as corruption
/// (issue #320, ADR-0045 §4). The op is kept in `ops`, unapplied, and counts
/// toward the sync cursor; `parked_ops` marks it and orders its replay after
/// an upgrade. Unlike `deferred_ops` it has no TTL and no cap, because what it
/// holds has been verified and cannot be fetched again.
///
/// `32` is migration `0032_routine_rrule_blob.sql`, which stores a routine's
/// recurrence rule as canonical CBOR beside its RFC 5545 text (issue #321,
/// ADR-0045 §6). A `FREQ`, `BYDAY` or `WKST` value this build does not know is
/// now kept verbatim, and a raw value holding `;`, `,` or `=` cannot survive
/// the text form; the blob holds any string. Schema-only: rows written before
/// it read their text, which only ever held known values.
pub const STORAGE_V: u16 = 32;

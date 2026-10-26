//! Shared id, enum and blob primitives.
//!
//! Every function here is pure, and each has consumers in several sibling
//! modules: `encode_unknowns` alone is called from the stream, context, task and
//! routine table regions, and `ms_to_ts` from nine of them. They are gathered
//! rather than duplicated, and they are the only items in `engine` that know
//! nothing about a table.

use super::{Engine, EngineError, META_STREAM};
use sunrise_domain::time::SunriseTime;
use sunrise_domain::{TaskState, PRIVATE_DOMAIN_BYTES};
use sunrise_id::{EntityKind, EntityRef, Ulid};

impl Engine {
    pub(super) fn fresh_id(&self, kind: EntityKind, now_ms: u64) -> EntityRef {
        let mut rand = [0u8; 10];
        self.rng.fill_bytes(&mut rand);
        let ulid = Ulid::from_timestamp_and_random(now_ms, rand);
        EntityRef::from_ulid(kind, ulid)
    }

    pub(super) fn fresh_op_id(&self, now_ms: u64) -> [u8; 16] {
        let mut rand = [0u8; 10];
        self.rng.fill_bytes(&mut rand);
        *Ulid::from_timestamp_and_random(now_ms, rand).as_bytes()
    }
}

// ---- encode/decode helpers ----

/// Encode an entity's preserved unknown fields for the `extra` column, or
/// `None` when there are none.
///
/// Hardcoding this column to `NULL` — which is what the three task writers did
/// until now — is how a v2 field arriving on a v1 device got dropped on the
/// floor: the entity carried it in memory for exactly one transaction, then the
/// materialized row forgot it and the next outbound op re-emitted the entity
/// without it. Every replica then converged on the truncated value.
pub(super) fn encode_unknowns(u: &sunrise_domain::Unknowns) -> rusqlite::Result<Option<Vec<u8>>> {
    if u.is_empty() {
        return Ok(None);
    }
    sunrise_cbor::encode_canonical(u)
        .map(Some)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

/// Decode the `extra` column back into an entity's unknown map.
///
/// A blob this build cannot parse reads as "no unknowns" rather than failing
/// the read: losing the whole Task over a column nobody here can interpret
/// would be worse. The read does not lose the blob, though. It stays in the
/// row, and [`extra_over_opaque`] keeps it there when the entity is written
/// back with nothing of its own to put in the column.
pub(super) fn decode_unknowns(blob: Option<Vec<u8>>) -> sunrise_domain::Unknowns {
    blob.and_then(|b| sunrise_cbor::decode_lenient(&b).ok())
        .unwrap_or_default()
}

/// A table whose rows carry an `extra` column that a write can overwrite.
#[derive(Debug, Clone, Copy)]
pub(super) enum ExtraTable {
    Streams,
    Contexts,
    Tasks,
    Routines,
    Blocks,
    Attachments,
}

impl ExtraTable {
    /// Every table, for the test that prepares each query against the schema.
    #[cfg(test)]
    pub(super) const ALL: [Self; 6] = [
        Self::Streams,
        Self::Contexts,
        Self::Tasks,
        Self::Routines,
        Self::Blocks,
        Self::Attachments,
    ];

    /// The query for a row's current `extra`. A constant per table, so no
    /// table name is ever formatted into SQL.
    pub(super) const fn select_extra(self) -> &'static str {
        match self {
            Self::Streams => "SELECT extra FROM streams WHERE stream_id = ?",
            Self::Contexts => "SELECT extra FROM contexts WHERE id = ?",
            Self::Tasks => "SELECT extra FROM tasks WHERE id = ?",
            Self::Routines => "SELECT extra FROM routines WHERE id = ?",
            Self::Blocks => "SELECT extra FROM blocks WHERE id = ?",
            Self::Attachments => "SELECT extra FROM attachments WHERE id = ?",
        }
    }

    /// The entity kind the table holds, for the log line.
    const fn kind(self) -> &'static str {
        match self {
            Self::Streams => "stream",
            Self::Contexts => "context",
            Self::Tasks => "task",
            Self::Routines => "routine",
            Self::Blocks => "block",
            Self::Attachments => "attachment",
        }
    }
}

/// The `extra` value to write over row `id` of `table`, given `new`, the
/// entity's own [`encode_unknowns`].
///
/// `new` when the entity carries unknown fields. When it carries none, and the
/// row already holds a blob this build cannot parse, that blob: it read as
/// "no unknowns" ([`decode_unknowns`]), and writing the empty map back would
/// delete a newer build's fields nobody here can see (ADR-0045 §6). The kept
/// blob is logged, because it means the row holds data this build cannot
/// read. Otherwise `None`.
///
/// An op from a peer whose entity carries no unknown fields keeps the blob
/// too. It is not the peer's data being resurrected: the blob was never
/// readable here, so nothing reads it, and a later write that carries unknown
/// fields of its own replaces it.
pub(super) fn extra_over_opaque(
    tx: &rusqlite::Connection,
    table: ExtraTable,
    id: &[u8],
    new: Option<Vec<u8>>,
) -> rusqlite::Result<Option<Vec<u8>>> {
    use rusqlite::OptionalExtension as _;
    if new.is_some() {
        return Ok(new);
    }
    let prior: Option<Vec<u8>> = tx
        .query_row(table.select_extra(), [id], |r| r.get(0))
        .optional()?
        .flatten();
    Ok(prior.filter(|b| {
        let opaque = sunrise_cbor::decode_lenient::<sunrise_domain::Unknowns>(b).is_err();
        if opaque {
            tracing::warn!(
                ev = "core.storage.extra_kept_opaque",
                kind = table.kind(),
                n_bytes = b.len(),
                "an entity's unknown-field column holds bytes this build cannot parse; \
                 kept them rather than writing an empty map over them"
            );
        }
        opaque
    }))
}

/// An optional [`SunriseTime`] as its four storage columns.
pub(super) type TimeParts<'a> = (Option<i64>, Option<&'a str>, Option<String>, Option<String>);

/// Split an optional [`SunriseTime`] into its four storage columns:
/// `(index_ms, kind, tz, civil)`.
///
/// The index key is what every range query and `ORDER BY` in the engine reads,
/// so adding kinds did not require touching one of them; the sidecars are what
/// let the kind survive the round trip, and the civil column what lets a civil
/// kind's wall-clock time survive it without a zone (migration 0041,
/// ADR-0066 §2). See the `tasks` table comment in `0013_baseline.sql`.
pub(super) fn time_to_parts(t: Option<&SunriseTime>) -> TimeParts<'_> {
    match t {
        None => (None, None, None, None),
        Some(v) => {
            let (ms, kind, tz) = v.to_parts();
            (
                Some(ms),
                Some(kind),
                tz.map(std::borrow::Cow::into_owned),
                v.civil_part(),
            )
        }
    }
}

/// Rebuild an optional [`SunriseTime`] from its four storage columns.
///
/// A row with an index key but no kind is read as an instant: that is what a
/// column written before the kind existed means, and what a build that does not
/// understand a future kind should fall back to.
pub(super) fn time_from_parts(
    ms: Option<i64>,
    kind: Option<&str>,
    tz: Option<&str>,
    civil: Option<&str>,
) -> Option<SunriseTime> {
    ms.map(|ms| {
        SunriseTime::from_parts(
            ms,
            kind.unwrap_or(sunrise_domain::time::kind::INSTANT),
            tz,
            civil,
        )
    })
}

/// Reject a caller-supplied Stream reference that names the vault-meta stream.
///
/// Every command that takes a `stream_id` from outside runs this, and it is
/// one function rather than a check per command for the same reason
/// `Engine::log_slot` is one function: the second hand-written copy of a rule
/// is where the rule starts drifting. `delete_stream` established the shape —
/// it refused the vault-meta id before anything else did — and this
/// generalises it from "you may not delete it" to "you may not name it".
///
/// **The private key domain is on this list too** (ADR-0046 §2). Its id is
/// the old Inbox sentinel's, and it is not a Stream: no stream command may
/// rename, re-parent, delete or nest under it. A command that gives a task or
/// block a home takes it through [`home_of`] instead, which reads it as "no
/// stream".
///
/// **`Command::RotateStreamKey` deliberately does not call this.** Rotating the
/// vault-meta key is a real operation — it is half of what a device revocation
/// does — and a caller with reason to believe that key is exposed must be able
/// to ask for it by name. It mints an epoch rather than routing an entity op,
/// so nothing about it competes for a sequence number it did not read.
///
/// # Errors
/// [`EngineError::Invalid`] if `r` is not a Stream at all;
/// [`EngineError::ReservedStream`] if it is the vault-meta stream or the
/// private domain.
pub(super) fn require_writable_stream(r: EntityRef) -> Result<(), EngineError> {
    require_kind(r, EntityKind::Stream)?;
    if r.bytes() == &META_STREAM || r.bytes() == &PRIVATE_DOMAIN_BYTES {
        return Err(EngineError::ReservedStream);
    }
    Ok(())
}

/// The home a caller-supplied stream names for a task, block or routine
/// template: `None` for no stream, which the legacy Inbox reference also
/// spells (ADR-0046 §5), and otherwise a stream [`require_writable_stream`]
/// accepts.
///
/// # Errors
/// As [`require_writable_stream`], for the vault-meta stream or a reference
/// that is not a Stream.
pub(super) fn home_of(r: Option<EntityRef>) -> Result<Option<EntityRef>, EngineError> {
    match r {
        Some(r) if r.kind() == EntityKind::Stream && r.bytes() == &PRIVATE_DOMAIN_BYTES => Ok(None),
        Some(r) => require_writable_stream(r).map(|()| Some(r)),
        None => Ok(None),
    }
}

/// The key domain whose 16-byte id seals a task or block op whose stream is
/// `home` (ADR-0046 §2): the stream's own, or the private domain's.
pub(super) fn domain_bytes(home: Option<EntityRef>) -> [u8; 16] {
    sunrise_domain::KeyDomain::of_home(home).bytes()
}

pub(super) fn require_kind(r: EntityRef, k: EntityKind) -> Result<(), EngineError> {
    if r.kind() != k {
        return Err(EngineError::Invalid(format!(
            "expected {k:?}, got {:?}",
            r.kind()
        )));
    }
    Ok(())
}

/// The stored spelling of a task state: the domain's wire spelling, so an
/// unknown state is stored as the raw string it arrived as.
pub(super) fn task_state_str(s: &TaskState) -> &str {
    s.as_str()
}

/// Read a stored task state.
///
/// Lossless, and delegated to the domain so the storage projection and the
/// wire decoder agree (ADR-0045 §6). A row written by a newer binary with a
/// state this build has never heard of reads back as that same unknown state
/// — which logic treats as `todo`, so the task is still there and still open
/// — rather than failing the whole read, and the next write of the row
/// carries the raw state through instead of overwriting it with `todo`.
pub(super) fn parse_task_state(s: &str) -> TaskState {
    TaskState::from_raw(s)
}

/// The stored spelling of an energy level; an unknown one's raw string.
pub(super) fn energy_str(e: &sunrise_domain::Energy) -> &str {
    e.as_str()
}

/// Read a stored energy level. Lossless: an unknown spelling is kept, where
/// it used to read as no energy at all and be written back as `NULL`.
pub(super) fn parse_energy(s: &str) -> sunrise_domain::Energy {
    sunrise_domain::Energy::from_raw(s)
}

pub(super) fn ms_to_ts(ms: i64) -> jiff::Timestamp {
    // Determinism: never read a wall clock on failure. Out-of-range epoch-ms
    // (corrupt row) clamps to the Unix epoch rather than `Timestamp::now()`.
    jiff::Timestamp::from_millisecond(ms).unwrap_or(jiff::Timestamp::UNIX_EPOCH)
}

/// The first four bytes of a 16-byte id as lowercase hex — **8 characters**,
/// not a full encoding.
///
/// The canonical producer of the `sender_h` and `subject_h` log fields (see
/// `sunrise_log::field`), and the short form every `Debug` impl in
/// [`crate::keychain`] prints.
///
/// Named for what it does, deliberately: the unqualified name `hex16`
/// elsewhere in this workspace (`sunrise_domain::export`,
/// `sunrise_core_bindings::dto`) takes the same `&[u8; 16]` and emits all
/// **32** characters. A `hex16` that emitted 8 read like a complete id in a
/// debug dump, which is why that spelling no longer exists here.
pub(crate) fn hex_short(b: &[u8; 16]) -> String {
    let mut s = String::with_capacity(8);
    for byte in b.iter().take(4) {
        use core::fmt::Write;
        let _ = write!(s, "{byte:02x}");
    }
    s
}

/// A whole blob as lowercase hex.
///
/// Distinct from [`hex_short`], which truncates to four bytes because what it
/// renders is an id and four bytes are enough to correlate one. This renders a
/// column value that is *not* an id — the wrong-width `stream_id` of a row a
/// revocation could not rotate — where truncating would throw away the only
/// thing that distinguishes one such row from another.
pub(crate) fn hex_bytes(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        use core::fmt::Write;
        let _ = write!(s, "{byte:02x}");
    }
    s
}

/// Zero-pad on the right or truncate a stored blob into a 16-byte id: the
/// blob's bytes go first, and a short one is followed by zeros.
///
/// The engine reads this from columns its own fold wrote, out of ids that
/// are 16 bytes by construction, so a blob of any other length is a corrupt
/// row. Padding one is the silent repair [`crate::keychain::to16`] argues
/// against, and is tolerated here only because refusing instead would change
/// what the engine does on such a row; moving these reads to `to16` is a
/// separate, behaviour-changing step.
pub(super) fn blob16(raw: &[u8]) -> [u8; 16] {
    let mut a = [0u8; 16];
    let take = raw.len().min(16);
    a[..take].copy_from_slice(&raw[..take]);
    a
}

/// Zero-pad on the right or truncate a stored blob into a 32-byte key or
/// digest, for the same reason and with the same caveat as [`blob16`].
pub(super) fn blob32(raw: &[u8]) -> [u8; 32] {
    let mut a = [0u8; 32];
    let take = raw.len().min(32);
    a[..take].copy_from_slice(&raw[..take]);
    a
}

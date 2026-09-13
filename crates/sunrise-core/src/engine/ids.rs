//! Shared id, enum and blob primitives.
//!
//! Every function here is pure, and each has consumers in several sibling
//! modules: `encode_unknowns` alone is called from the stream, context, task and
//! routine table regions, and `ms_to_ts` from nine of them. They are gathered
//! rather than duplicated, and they are the only items in `engine` that know
//! nothing about a table.

use super::{Engine, EngineError};
use sunrise_domain::time::SunriseTime;
use sunrise_domain::TaskState;
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
/// A blob this build cannot parse degrades to "no unknowns" rather than
/// failing the read: losing a field nobody here can interpret is bad, and
/// losing the whole Task because of it is worse.
pub(super) fn decode_unknowns(blob: Option<Vec<u8>>) -> sunrise_domain::Unknowns {
    blob.and_then(|b| sunrise_cbor::decode_lenient(&b).ok())
        .unwrap_or_default()
}

/// Split an optional [`SunriseTime`] into its three storage columns:
/// `(index_ms, kind, tz)`.
///
/// The index key is what every range query and `ORDER BY` in the engine reads,
/// so adding kinds did not require touching one of them; the two sidecars are
/// what let the kind survive the round trip. See the `tasks` table comment in
/// `0013_baseline.sql`.
pub(super) fn time_to_parts(
    t: Option<&SunriseTime>,
) -> (Option<i64>, Option<&'static str>, Option<String>) {
    match t {
        None => (None, None, None),
        Some(v) => {
            let (ms, kind, tz) = v.to_parts();
            (Some(ms), Some(kind), tz.map(ToOwned::to_owned))
        }
    }
}

/// Rebuild an optional [`SunriseTime`] from its three storage columns.
///
/// A row with an index key but no kind is read as an instant: that is what a
/// column written before the kind existed means, and what a build that does not
/// understand a future kind should fall back to.
pub(super) fn time_from_parts(
    ms: Option<i64>,
    kind: Option<&str>,
    tz: Option<&str>,
) -> Option<SunriseTime> {
    ms.map(|ms| {
        SunriseTime::from_parts(ms, kind.unwrap_or(sunrise_domain::time::kind::INSTANT), tz)
    })
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

pub(super) fn task_state_str(s: TaskState) -> &'static str {
    match s {
        TaskState::Todo => "todo",
        TaskState::InProgress => "in_progress",
        TaskState::Done => "done",
        TaskState::Cancelled => "cancelled",
    }
}

/// Read a stored task state.
///
/// Lossy on purpose, and delegated to the domain so the storage projection and
/// the wire decoder degrade the same way. A row written by a newer binary with
/// a state this build has never heard of reads as `todo` — the task is still
/// there and still open — rather than failing the whole read and taking every
/// query that touches it down with it.
pub(super) fn parse_task_state(s: &str) -> TaskState {
    TaskState::from_str_lossy(s)
}

pub(super) fn energy_str(e: sunrise_domain::Energy) -> &'static str {
    match e {
        sunrise_domain::Energy::Low => "low",
        sunrise_domain::Energy::Med => "med",
        sunrise_domain::Energy::High => "high",
    }
}

pub(super) fn parse_energy(s: &str) -> Option<sunrise_domain::Energy> {
    match s {
        "low" => Some(sunrise_domain::Energy::Low),
        "med" => Some(sunrise_domain::Energy::Med),
        "high" => Some(sunrise_domain::Energy::High),
        _ => None,
    }
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

/// Left-pad / truncate a stored blob into a 16-byte id.
pub(super) fn blob16(raw: &[u8]) -> [u8; 16] {
    let mut a = [0u8; 16];
    let take = raw.len().min(16);
    a[..take].copy_from_slice(&raw[..take]);
    a
}

/// Left-pad / truncate a stored blob into a 32-byte key or digest.
pub(super) fn blob32(raw: &[u8]) -> [u8; 32] {
    let mut a = [0u8; 32];
    let take = raw.len().min(32);
    a[..take].copy_from_slice(&raw[..take]);
    a
}

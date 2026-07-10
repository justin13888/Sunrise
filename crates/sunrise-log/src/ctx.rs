//! Per-record context (`ctx`) keys and the redaction allowlist.
//!
//! Per `docs/10-cross-cutting/logging.md` §6, the context object MUST only
//! contain keys from the allowlist below; unknown keys are dropped at the
//! sink. Plaintext domain data (Task title, Note body, email, etc.) is
//! ALWAYS forbidden — it doesn't appear here because it can't pass through
//! the [`Plain<T>`] gate.

use serde::Serialize;
use smallvec::SmallVec;

/// Allowed keys for the `ctx` object in a log record.
///
/// Per logging.md §6, this is the full v1 list. Adding a key requires a doc
/// update + an update to `Self::canonical_name`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CtxKey {
    /// `BLAKE3(stream_id || device_log_salt, 4)` lowercase hex; per-device,
    /// not correlatable across devices.
    StreamH,
    /// `BLAKE3(task_id || device_log_salt, 4)` lowercase hex.
    TaskH,
    /// `BLAKE3(block_id || device_log_salt, 4)` lowercase hex.
    BlockH,
    /// `BLAKE3(routine_id || device_log_salt, 4)` lowercase hex.
    RoutineH,
    /// `BLAKE3(note_id || device_log_salt, 4)` lowercase hex.
    NoteH,
    /// `BLAKE3(attachment_id || device_log_salt, 4)` lowercase hex.
    AttachmentH,
    /// `BLAKE3(person_id || device_log_salt, 4)` lowercase hex.
    PersonH,
    /// `BLAKE3(account_id || log_salt, 4)` lowercase hex.
    AccountH,
    /// Stream-key rotation epoch.
    Epoch,
    /// Op sequence number.
    Seq,
    /// Op kind discriminator (e.g. `"task.update"`); structural, not content.
    OpKind,
    /// Generic count of ops.
    NOps,
    /// Generic count of chunks.
    NChunks,
    /// Generic byte counter.
    NBytes,
    /// Generic count of recipients/devices.
    NDevices,
    /// Generic count of imported items (integrations).
    NImported,
    /// Generic count of exported items (integrations).
    NExported,
    /// Latency in milliseconds.
    LatMs,
    /// Billing tier (`"free"` / `"pro"` / ...).
    Tier,
    /// External provider (`"apns"` / `"fcm"` / `"web"` / `"google_calendar"` / ...).
    Provider,
    /// Result enum (`"ok"` / `"failed"` / `"skipped"`).
    Result,
    /// Schema version (e.g. `from_v` / `to_v` for migrations).
    FromV,
    /// Schema version (target).
    ToV,
    /// HTTP status code.
    Status,
    /// HTTP endpoint (templated, e.g. `/api/v1/blobs/:id`).
    Endpoint,
    /// Action kind (UI surface).
    ActionKind,
    /// View name (UI surface).
    View,
    /// Wire-frame kind discriminator.
    Kind,
    /// AEAD algorithm id.
    AeadAlg,
    /// Signature algorithm id.
    SigAlg,
    /// Whether the record was throttled (number dropped before this `log.throttled`).
    NDropped,
}

impl CtxKey {
    /// Canonical lowercase JSON key name.
    #[must_use]
    pub const fn canonical_name(self) -> &'static str {
        match self {
            Self::StreamH => "stream_h",
            Self::TaskH => "task_h",
            Self::BlockH => "block_h",
            Self::RoutineH => "routine_h",
            Self::NoteH => "note_h",
            Self::AttachmentH => "attachment_h",
            Self::PersonH => "person_h",
            Self::AccountH => "account_h",
            Self::Epoch => "epoch",
            Self::Seq => "seq",
            Self::OpKind => "op_kind",
            Self::NOps => "n_ops",
            Self::NChunks => "n_chunks",
            Self::NBytes => "n_bytes",
            Self::NDevices => "n_devices",
            Self::NImported => "n_imported",
            Self::NExported => "n_exported",
            Self::LatMs => "lat_ms",
            Self::Tier => "tier",
            Self::Provider => "provider",
            Self::Result => "result",
            Self::FromV => "from_v",
            Self::ToV => "to_v",
            Self::Status => "status",
            Self::Endpoint => "endpoint",
            Self::ActionKind => "action_kind",
            Self::View => "view",
            Self::Kind => "kind",
            Self::AeadAlg => "aead_alg",
            Self::SigAlg => "sig_alg",
            Self::NDropped => "n_dropped",
        }
    }
}

/// Concrete value attached to a [`CtxKey`].
///
/// `Plain<T>` is intentionally NOT a variant: plaintext user data may not
/// land in any log surface. The redaction property test enforces this by
/// generating synthetic `Plain<T>` payloads and asserting no synthetic byte
/// appears in any sink output.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum CtxValue {
    /// A short, structural string (enum-like discriminators, hex hashes,
    /// templated paths). Must NOT contain user-authored content; the caller
    /// is responsible.
    Str(&'static str),
    /// A short owned string (e.g. computed hash); same content rules.
    String(String),
    /// Unsigned integer (counts, sizes, latencies, statuses).
    U64(u64),
    /// Signed integer.
    I64(i64),
    /// Boolean flag.
    Bool(bool),
}

impl Serialize for CtxValue {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Str(s) => ser.serialize_str(s),
            Self::String(s) => ser.serialize_str(s),
            Self::U64(v) => ser.serialize_u64(*v),
            Self::I64(v) => ser.serialize_i64(*v),
            Self::Bool(v) => ser.serialize_bool(*v),
        }
    }
}

/// Per-record context map. Stored as a small inline vec to avoid heap
/// allocation for the common 0-3 entries case.
#[derive(Debug, Default, Clone)]
pub struct Ctx {
    entries: SmallVec<[(CtxKey, CtxValue); 4]>,
}

impl Ctx {
    /// Empty context.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert `(key, value)`.
    #[must_use]
    pub fn with(mut self, key: CtxKey, value: CtxValue) -> Self {
        self.entries.push((key, value));
        self
    }

    /// All entries, in insertion order.
    pub fn entries(&self) -> impl Iterator<Item = (CtxKey, &CtxValue)> + '_ {
        self.entries.iter().map(|(k, v)| (*k, v))
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the context is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Serialize for Ctx {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = ser.serialize_map(Some(self.entries.len()))?;
        for (k, v) in &self.entries {
            map.serialize_entry(k.canonical_name(), v)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctx_round_trips_structural_values() {
        let ctx = Ctx::new()
            .with(CtxKey::StreamH, CtxValue::Str("abc123"))
            .with(CtxKey::Epoch, CtxValue::U64(4))
            .with(CtxKey::OpKind, CtxValue::Str("task.update"));
        let s = serde_json::to_string(&ctx).unwrap();
        assert!(s.contains("\"stream_h\":\"abc123\""));
        assert!(s.contains("\"epoch\":4"));
        assert!(s.contains("\"op_kind\":\"task.update\""));
    }

    #[test]
    fn empty_ctx_is_empty_object() {
        let ctx = Ctx::new();
        let s = serde_json::to_string(&ctx).unwrap();
        assert_eq!(s, "{}");
    }

    #[test]
    fn canonical_names_are_unique_and_lowercase() {
        let all = [
            CtxKey::StreamH,
            CtxKey::TaskH,
            CtxKey::BlockH,
            CtxKey::RoutineH,
            CtxKey::NoteH,
            CtxKey::AttachmentH,
            CtxKey::PersonH,
            CtxKey::AccountH,
            CtxKey::Epoch,
            CtxKey::Seq,
            CtxKey::OpKind,
            CtxKey::NOps,
            CtxKey::NChunks,
            CtxKey::NBytes,
            CtxKey::NDevices,
            CtxKey::NImported,
            CtxKey::NExported,
            CtxKey::LatMs,
            CtxKey::Tier,
            CtxKey::Provider,
            CtxKey::Result,
            CtxKey::FromV,
            CtxKey::ToV,
            CtxKey::Status,
            CtxKey::Endpoint,
            CtxKey::ActionKind,
            CtxKey::View,
            CtxKey::Kind,
            CtxKey::AeadAlg,
            CtxKey::SigAlg,
            CtxKey::NDropped,
        ];
        let mut names: Vec<&str> = all.iter().map(|k| k.canonical_name()).collect();
        names.sort_unstable();
        let pre = names.len();
        names.dedup();
        assert_eq!(pre, names.len(), "ctx key names not unique");
        for n in &names {
            assert!(
                n.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "ctx key {n:?} not [a-z0-9_]"
            );
        }
    }
}

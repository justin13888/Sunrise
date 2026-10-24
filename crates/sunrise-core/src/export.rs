//! The full decrypted export: every live record, laid out as files in an open
//! format (`docs/04-storage/export-format.md`, issue #350).
//!
//! This module decides **what** the export holds and how each file is
//! encoded. It touches no filesystem (`docs/01-architecture/shared-core.md`
//! §Determinism rules, rule 2): [`entity_files`] returns paths and bytes, and
//! the writer that puts them under a directory, reads attachment bytes one at
//! a time and writes `manifest.json` last is the client layer's
//! (`sunrise-client-core`'s `export`).
//!
//! Every byte is a function of the records and of the instant and zone the
//! caller passes, so the same vault exported at the same instant gives the
//! same bytes: rows are ordered by id, object keys are sorted, and files are
//! listed by path.
//!
//! Nothing secret is written. A record's only key material is an attachment's
//! `blob_key` and `thumbnail_blob_key` ([`ATTACHMENT_SECRET_FIELDS`]), which
//! are removed. Keys, credentials, device certificates and the op log are not
//! records and never reach this module.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sunrise_domain::{
    Attachment, Context, FocusSession, NoteBody, Preferences, ReviewSnapshot, Routine, SavedView,
    Stream, Task,
};
use sunrise_id::EntityRef;

use crate::queries::BlockRow;

/// The `format` a manifest names. Stable: a reader keys on it.
pub const EXPORT_FORMAT: &str = "sunrise-export";

/// The export format's version. Raised when a file's meaning or layout
/// changes in a way an existing reader would misread; a field added to a
/// record is not such a change, because records follow `doc_schema_v`.
pub const EXPORT_FORMAT_V: u32 = 1;

/// Where the manifest is written, relative to the export root.
pub const MANIFEST_PATH: &str = "manifest.json";

/// Where the calendar of every Block is written, relative to the export root.
pub const CALENDAR_PATH: &str = "calendar.ics";

/// The fields of an attachment record that are key material. They open the
/// blob, so they stay in the vault; the export carries the bytes instead.
pub const ATTACHMENT_SECRET_FIELDS: [&str; 2] = ["blob_key", "thumbnail_blob_key"];

/// The entity kinds the export writes a file for, by registry tag
/// (`sunrise_id::registry::ENTITIES`): one `entities/<tag>.jsonl` each,
/// written even when it holds no row.
///
/// Every registered kind whose first record has its own table is here, and a
/// test holds the two lists together, so a new stored kind fails the build of
/// the tests until the export carries it. `note` and `person` have no table of
/// their own (a note is a task's, stream's or routine's body), and `device`
/// and `identity` are the account's membership, not its data.
pub const EXPORTED_KINDS: [&str; 10] = [
    "task",
    "stream",
    "context",
    "routine",
    "block",
    "attachment",
    "focus_session",
    "review_snapshot",
    "preferences",
    "saved_view",
];

/// Every live record this replica holds, as it materializes them.
///
/// Tombstoned rows are not here: the export is the data, not the log.
#[derive(Debug, Clone, Default)]
pub struct VaultRecords {
    /// Live tasks.
    pub tasks: Vec<Task>,
    /// Live streams, without the Inbox (which is not a Stream entity).
    pub streams: Vec<Stream>,
    /// Live contexts.
    pub contexts: Vec<Context>,
    /// Live routines.
    pub routines: Vec<Routine>,
    /// Live blocks, each with the title it shows.
    pub blocks: Vec<BlockRow>,
    /// Live attachments' metadata.
    pub attachments: Vec<Attachment>,
    /// Every focus session, running ones included.
    pub focus_sessions: Vec<FocusSession>,
    /// Every saved review snapshot.
    pub review_snapshots: Vec<ReviewSnapshot>,
    /// The synced preferences record, once anything has been set.
    pub preferences: Option<Preferences>,
    /// Live saved views (#341), by id.
    pub saved_views: Vec<SavedView>,
}

/// One file of an export: a path relative to the export root, always with
/// `/` separators, and its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportFile {
    /// Relative path, `/`-separated.
    pub path: String,
    /// Contents.
    pub bytes: Vec<u8>,
}

/// Why a record could not be written.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// A record did not encode as JSON. Not expected for any record this build
    /// reads; reported rather than skipped, because a silently short export is
    /// the failure this format exists to prevent.
    #[error("could not encode {kind} {id} as JSON: {reason}")]
    Encode {
        /// Registry tag.
        kind: &'static str,
        /// The record's id.
        id: String,
        /// The encoder's message.
        reason: String,
    },
    /// A manifest did not parse.
    #[error("not a {EXPORT_FORMAT} manifest: {0}")]
    Manifest(String),
}

/// The export's table of contents (`manifest.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Always [`EXPORT_FORMAT`].
    pub format: String,
    /// [`EXPORT_FORMAT_V`] at write time.
    pub format_v: u32,
    /// The document schema the records are written in (`DOC_SCHEMA_V`).
    pub doc_schema_v: u16,
    /// That schema's registered fingerprint, lowercase hex, when this build
    /// has one registered.
    pub doc_schema_fingerprint: Option<String>,
    /// The vault's storage version (`STORAGE_V`), for a reader comparing
    /// exports of one vault over time.
    pub storage_v: u16,
    /// When the export was taken, RFC 9557 (an instant, `Z`).
    pub exported_at: String,
    /// The same instant, milliseconds since the epoch.
    pub exported_at_ms: u64,
    /// The reader's zone the export was taken in. Informational: no file
    /// depends on it, because every time keeps its own kind.
    pub zone: String,
    /// The Inbox's stream id: a task whose `stream_id` is this one is in the
    /// Inbox, which has no row of its own in `entities/stream.jsonl`.
    pub inbox_stream: String,
    /// Every file written except this one, by path.
    pub files: Vec<ManifestFile>,
    /// Every live attachment, by id, and whether its bytes are in the export.
    pub attachments: Vec<ManifestAttachment>,
}

/// One file the manifest vouches for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFile {
    /// Relative path, `/`-separated.
    pub path: String,
    /// Length in bytes.
    pub size: u64,
    /// BLAKE3 of the contents, lowercase hex.
    pub blake3: String,
}

impl ManifestFile {
    /// The entry for `bytes` written at `path`.
    #[must_use]
    pub fn of(path: &str, bytes: &[u8]) -> Self {
        Self {
            path: path.to_owned(),
            size: bytes.len() as u64,
            blake3: digest_hex(bytes),
        }
    }

    /// Whether `bytes` are what this entry describes.
    #[must_use]
    pub fn matches(&self, bytes: &[u8]) -> bool {
        self.size == bytes.len() as u64 && self.blake3 == digest_hex(bytes)
    }
}

/// Whether an attachment's bytes made it into the export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentState {
    /// Written at the entry's `path`, and listed in `files`.
    Present,
    /// Not written; `missing_reason` says why.
    Missing,
}

/// Why an attachment's bytes are not in the export.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingReason {
    /// Not on this device, and the export was asked not to fetch.
    NotFetched,
    /// No device ever uploaded them, so no relay holds them.
    NeverUploaded,
    /// A fetch was asked for and did not bring them.
    FetchFailed,
}

/// One live attachment in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestAttachment {
    /// The attachment's id.
    pub id: String,
    /// The entity it hangs off.
    pub parent: String,
    /// Its filename as stored.
    pub filename: String,
    /// Its MIME type.
    pub mime_type: String,
    /// Its plaintext size.
    pub size_bytes: u64,
    /// Whether the bytes are in the export.
    pub state: AttachmentState,
    /// Where they are, when present: [`attachment_path`].
    pub path: Option<String>,
    /// Why not, when missing.
    pub missing_reason: Option<MissingReason>,
}

impl ManifestAttachment {
    /// The entry for `att`, present at [`attachment_path`] or missing for
    /// `reason`.
    #[must_use]
    pub fn new(att: &Attachment, missing: Option<MissingReason>) -> Self {
        Self {
            id: att.id.to_str(),
            parent: att.parent.to_str(),
            filename: att.filename.clone(),
            mime_type: att.mime_type.clone(),
            size_bytes: att.size_bytes,
            state: if missing.is_some() {
                AttachmentState::Missing
            } else {
                AttachmentState::Present
            },
            path: missing.is_none().then(|| attachment_path(att)),
            missing_reason: missing,
        }
    }
}

impl Manifest {
    /// A manifest for an export taken at `exported_at_ms` in `zone`, with the
    /// build's format and schema versions and no files yet.
    #[must_use]
    pub fn new(exported_at_ms: u64, zone: &str) -> Self {
        use sunrise_cbor::version::{doc_schema_fingerprint, DOC_SCHEMA_V, STORAGE_V};
        let at = i64::try_from(exported_at_ms)
            .ok()
            .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
            .unwrap_or(jiff::Timestamp::UNIX_EPOCH);
        Self {
            format: EXPORT_FORMAT.to_owned(),
            format_v: EXPORT_FORMAT_V,
            doc_schema_v: DOC_SCHEMA_V,
            doc_schema_fingerprint: doc_schema_fingerprint(u32::from(DOC_SCHEMA_V))
                .map(|fp| hex(&fp)),
            storage_v: STORAGE_V,
            exported_at: at.to_string(),
            exported_at_ms,
            zone: zone.to_owned(),
            inbox_stream: sunrise_domain::inbox_stream_ref().to_str(),
            files: Vec::new(),
            attachments: Vec::new(),
        }
    }

    /// The manifest as written: sorted keys, two-space indent, and a final
    /// newline. Files and attachments are sorted first, so the order they
    /// were added in does not reach the bytes.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut m = self.clone();
        m.files.sort_by(|a, b| a.path.cmp(&b.path));
        m.attachments.sort_by(|a, b| a.id.cmp(&b.id));
        // Every field is a string, an integer, a list or an option of those.
        let v = sorted(serde_json::to_value(&m).unwrap_or(Value::Null));
        let mut out = serde_json::to_string_pretty(&v)
            .unwrap_or_default()
            .into_bytes();
        out.push(b'\n');
        out
    }

    /// Read a manifest back.
    ///
    /// # Errors
    ///
    /// [`ExportError::Manifest`] when the bytes are not JSON of this shape or
    /// name another format.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ExportError> {
        let m: Self =
            serde_json::from_slice(bytes).map_err(|e| ExportError::Manifest(e.to_string()))?;
        if m.format != EXPORT_FORMAT {
            return Err(ExportError::Manifest(format!("format is {:?}", m.format)));
        }
        Ok(m)
    }

    /// The entry for `path`, if the manifest lists it.
    #[must_use]
    pub fn file(&self, path: &str) -> Option<&ManifestFile> {
        self.files.iter().find(|f| f.path == path)
    }
}

/// BLAKE3 of `bytes`, lowercase hex.
#[must_use]
pub fn digest_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Where an attachment's bytes go: `attachments/<id>/<filename>`, the
/// filename made safe to create on any filesystem by [`safe_filename`].
///
/// The id directory is what keeps two attachments called `scan.pdf` apart.
#[must_use]
pub fn attachment_path(att: &Attachment) -> String {
    format!(
        "attachments/{}/{}",
        att.id.to_str(),
        safe_filename(&att.filename)
    )
}

/// `name` as one path component every filesystem accepts.
///
/// A filename is a peer's text, so it may hold a separator, `..`, a control
/// character or a character Windows refuses; each becomes `_`, leading dots
/// and trailing dots and spaces go, the result is at most 200 bytes, and an
/// empty result is `file`. Nothing here can name a path outside the
/// attachment's own directory.
#[must_use]
pub fn safe_filename(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = mapped
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .trim_start();
    let mut out = String::new();
    for c in trimmed.chars() {
        if out.len() + c.len_utf8() > 200 {
            break;
        }
        out.push(c);
    }
    if out.is_empty() {
        "file".to_owned()
    } else {
        out
    }
}

/// `v` with every object's keys in sorted order, whatever map the JSON
/// library was built with.
fn sorted(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut entries: Vec<(String, Value)> = m.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut out = Map::new();
            for (k, v) in entries {
                out.insert(k, sorted(v));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// One record as a JSONL line: its document-schema JSON with sorted keys.
fn line<T: Serialize>(kind: &'static str, id: EntityRef, rec: &T) -> Result<Value, ExportError> {
    serde_json::to_value(rec)
        .map(sorted)
        .map_err(|e| ExportError::Encode {
            kind,
            id: id.to_str(),
            reason: e.to_string(),
        })
}

/// `entities/<kind>.jsonl` from rows already encoded, ordered by id.
fn jsonl(kind: &'static str, mut rows: Vec<(EntityRef, Value)>) -> ExportFile {
    rows.sort_by(|a, b| a.0.bytes().cmp(b.0.bytes()));
    let mut bytes = Vec::new();
    for (_, v) in rows {
        // A `Value` always serializes.
        bytes.extend_from_slice(serde_json::to_string(&v).unwrap_or_default().as_bytes());
        bytes.push(b'\n');
    }
    ExportFile {
        path: format!("entities/{kind}.jsonl"),
        bytes,
    }
}

fn rows<'a, T: Serialize + 'a>(
    kind: &'static str,
    items: impl IntoIterator<Item = (EntityRef, &'a T)>,
) -> Result<ExportFile, ExportError> {
    let encoded = items
        .into_iter()
        .map(|(id, rec)| line(kind, id, rec).map(|v| (id, v)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(jsonl(kind, encoded))
}

/// An attachment's line: its record without [`ATTACHMENT_SECRET_FIELDS`].
fn attachment_line(att: &Attachment) -> Result<Value, ExportError> {
    let mut v = line("attachment", att.id, att)?;
    if let Value::Object(m) = &mut v {
        for field in ATTACHMENT_SECRET_FIELDS {
            m.remove(field);
        }
    }
    Ok(v)
}

/// A note's Markdown projection, `notes/<owner id>.md`, when it has one.
fn note_file(owner: EntityRef, body: Option<&NoteBody>) -> Option<ExportFile> {
    let body = body.filter(|b| !b.is_empty())?;
    let doc = sunrise_domain::decode_note_body(body);
    let mut md = sunrise_domain::note_body_to_markdown(&doc.blocks);
    if !md.ends_with('\n') {
        md.push('\n');
    }
    Some(ExportFile {
        path: format!("notes/{}.md", owner.to_str()),
        bytes: md.into_bytes(),
    })
}

/// Every file the records make: one `entities/<kind>.jsonl` per
/// [`EXPORTED_KINDS`] entry and one `notes/<id>.md` per non-empty task body,
/// stream description or routine template body, sorted by path.
///
/// Not the calendar (the iCalendar writer is a client-layer dependency), not
/// attachment bytes (read one at a time by the writer), and not the manifest.
///
/// # Errors
///
/// [`ExportError::Encode`] if a record does not encode.
pub fn entity_files(records: &VaultRecords) -> Result<Vec<ExportFile>, ExportError> {
    let mut out = vec![
        rows("task", records.tasks.iter().map(|t| (t.id, t)))?,
        rows("stream", records.streams.iter().map(|s| (s.id, s)))?,
        rows("context", records.contexts.iter().map(|c| (c.id, c)))?,
        rows("routine", records.routines.iter().map(|r| (r.id, r)))?,
        rows(
            "block",
            records.blocks.iter().map(|b| (b.block.id, &b.block)),
        )?,
        jsonl(
            "attachment",
            records
                .attachments
                .iter()
                .map(|a| attachment_line(a).map(|v| (a.id, v)))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        rows(
            "focus_session",
            records.focus_sessions.iter().map(|f| (f.start.id, f)),
        )?,
        rows(
            "review_snapshot",
            records.review_snapshots.iter().map(|r| (r.id, r)),
        )?,
        rows("preferences", records.preferences.iter().map(|p| (p.id, p)))?,
        rows("saved_view", records.saved_views.iter().map(|v| (v.id, v)))?,
    ];
    out.extend(
        records
            .tasks
            .iter()
            .filter_map(|t| note_file(t.id, t.body.as_ref())),
    );
    out.extend(
        records
            .streams
            .iter()
            .filter_map(|s| note_file(s.id, s.description.as_ref())),
    );
    out.extend(
        records
            .routines
            .iter()
            .filter_map(|r| note_file(r.id, r.template.body.as_ref())),
    );
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

#[cfg(test)]
mod tests;

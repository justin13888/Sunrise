//! NDJSON record shape per `spec/10-cross-cutting/logging.md` §3.

use crate::{ctx::Ctx, level::Level, proto::ProtoVersions};
use serde::{Deserialize, Serialize};

/// Error category attached to `warn` / `error` records.
///
/// Per `spec/10-cross-cutting/error-handling.md` §kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ErrorKind {
    /// Will likely succeed if retried.
    Transient,
    /// Will not succeed without intervention.
    Permanent,
    /// User input was invalid.
    User,
    /// A bug in our code.
    Internal,
}

/// `err` field on `warn` / `error` records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrField {
    /// Stable canonical error code.
    pub code: &'static str,
    /// Error category.
    pub kind: ErrorKind,
    /// Whether the operation may be retried.
    pub retryable: bool,
    /// Bottom-most `source()` chain after redaction; ≤ 500 bytes; MUST NOT
    /// contain plaintext user data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
}

/// One log record (the wire NDJSON object).
///
/// Field order is the canonical order from logging.md §3 to keep visual
/// review easy. JSON serializes maps in insertion order via `serde_json`.
#[derive(Debug, Clone, Serialize)]
pub struct Record<'a> {
    /// RFC 3339 with millisecond precision, UTC, `Z` suffix.
    pub ts: String,
    /// Level (lowercase).
    pub lv: Level,
    /// Hierarchical event name.
    pub ev: &'a str,
    /// Cargo crate / npm package / platform module.
    pub pkg: &'a str,
    /// Rust module path / TS file / Swift / Kotlin path.
    #[serde(rename = "mod")]
    pub mod_path: &'a str,
    /// Span ULID.
    pub span: String,
    /// Trace ULID.
    pub trace: String,
    /// `dev_<8 hex>` device-local hash.
    pub dev: String,
    /// `<semver>+<platform>`.
    pub app: String,
    /// Numeric protocol versions.
    pub proto: ProtoVersions,
    /// Free-form context — keys come from the redaction allowlist.
    pub ctx: &'a Ctx,
    /// Human-readable summary, ≤ 200 bytes.
    pub msg: &'a str,
    /// Error envelope (warn / error only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub err: Option<&'a ErrField>,
    /// Marker that this record is opt-in for upload to remote sink.
    #[serde(skip_serializing_if = "is_false")]
    pub share: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::{CtxKey, CtxValue};

    #[test]
    fn record_serializes_canonical_shape() {
        let ctx = Ctx::new().with(CtxKey::StreamH, CtxValue::Str("abc123"));
        let r = Record {
            ts: "2026-05-08T12:34:56.789Z".to_string(),
            lv: Level::Info,
            ev: "sync.session.opened",
            pkg: "sunrise-sync",
            mod_path: "sunrise_sync::session",
            span: "01HXYZ".to_string(),
            trace: "01HXYZ".to_string(),
            dev: "dev_abcdef01".to_string(),
            app: "1.4.2+macos".to_string(),
            proto: ProtoVersions {
                wire: 1,
                doc: 1,
                crypto: 1,
            },
            ctx: &ctx,
            msg: "session opened",
            err: None,
            share: false,
        };
        let s = serde_json::to_string(&r).unwrap();
        // Required fields present.
        for fld in [
            "\"ts\":",
            "\"lv\":\"info\"",
            "\"ev\":\"sync.session.opened\"",
            "\"pkg\":\"sunrise-sync\"",
            "\"mod\":\"sunrise_sync::session\"",
            "\"span\":\"01HXYZ\"",
            "\"trace\":\"01HXYZ\"",
            "\"dev\":\"dev_abcdef01\"",
            "\"app\":\"1.4.2+macos\"",
            "\"proto\":",
            "\"ctx\":{\"stream_h\":\"abc123\"}",
            "\"msg\":\"session opened\"",
        ] {
            assert!(s.contains(fld), "missing {fld:?} in {s}");
        }
        // err / share are absent because they're None / false.
        assert!(!s.contains("\"err\":"));
        assert!(!s.contains("\"share\":"));
    }

    #[test]
    fn err_field_serializes_with_kind() {
        let err = ErrField {
            code: "SYNC_AUTH_REJECTED",
            kind: ErrorKind::Transient,
            retryable: true,
            cause: Some("OIDC token expired".to_string()),
        };
        let s = serde_json::to_string(&err).unwrap();
        assert!(s.contains("\"code\":\"SYNC_AUTH_REJECTED\""));
        assert!(s.contains("\"kind\":\"transient\""));
        assert!(s.contains("\"retryable\":true"));
        assert!(s.contains("\"cause\":\"OIDC token expired\""));
    }
}

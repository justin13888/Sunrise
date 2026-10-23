//! The client settings file: device-local settings with no key in the core's
//! key table.
//!
//! Each of these is a fact about one install, never synced, and none of them
//! is OS-specific, so they are kept here rather than in each client's own
//! store (`UserDefaults` on Apple):
//!
//! * the **Focus filter's scope** — the streams the OS Focus in force picked
//!   ([`crate::focus`]). Persisted because iOS runs the filter's intent when
//!   a Focus changes, which may launch the app in the background and end it
//!   again long before anyone opens it;
//! * the **account address** sent once when a vault is published;
//! * whether **keyboard tips** are shown.
//!
//! A preference with a key in the table (the notification switches, the vim
//! mode, the attachment cache) belongs to the core's `Preferences`
//! ([`sunrise_core::Query::Preferences`]), not here. Adding these three to the
//! table instead would be a registry change every replica reads, for settings
//! no other replica needs.
//!
//! The file is JSON, written whole and atomically like the bootstrap file. A
//! field this build does not know is kept and written back unchanged, so an
//! older build never drops a newer build's setting.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use sunrise_id::{EntityKind, EntityRef};

use super::{write_atomically, PreferenceFileError};

/// The file's name, beside the bootstrap file in the directory the client
/// names.
pub const CLIENT_SETTINGS_FILE: &str = "client-settings.json";

/// One stream a Focus filter picked, with the name it was picked under — kept
/// so a filter can be described when the vault cannot be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusStream {
    /// The stream's id, as text (`str_…`).
    pub stream: String,
    /// Its name when it was picked.
    pub name: String,
}

/// Every client setting.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientSettings {
    /// The Focus filter in force; empty is no filter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub focus_filter: Vec<FocusStream>,
    /// The address an account is registered under, sent once when the
    /// account is created. Absent until set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_email: Option<String>,
    /// Whether keyboard tips are shown; absent reads as off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard_tips: Option<bool>,
    /// Fields a newer build wrote, kept as they came.
    #[serde(flatten)]
    other: BTreeMap<String, serde_json::Value>,
}

impl ClientSettings {
    /// The Focus filter's scope: `None` with no filter, and never an empty
    /// set ([`crate::focus::scope`]). An id that does not parse as a stream is
    /// skipped.
    #[must_use]
    pub fn focus_scope(&self) -> Option<BTreeSet<EntityRef>> {
        crate::focus::scope(
            self.focus_filter
                .iter()
                .filter_map(|f| EntityRef::parse(&f.stream, EntityKind::Stream).ok()),
        )
    }

    /// Put a Focus filter in force, or lift it with an empty list. A stream
    /// picked twice keeps its first name.
    pub fn set_focus_filter(&mut self, picked: Vec<FocusStream>) {
        let mut seen = BTreeSet::new();
        self.focus_filter = picked
            .into_iter()
            .filter(|f| seen.insert(f.stream.clone()))
            .collect();
    }

    /// Set the account address, trimmed; blank clears it.
    pub fn set_account_email(&mut self, email: Option<&str>) {
        self.account_email = email
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(str::to_owned);
    }
}

/// The client settings held in `dir`; the defaults when there is no file.
///
/// # Errors
///
/// [`PreferenceFileError::Io`] for a file that exists and cannot be read, and
/// [`PreferenceFileError::Malformed`] for one that is not a settings object.
pub fn read_client_settings(dir: &Path) -> Result<ClientSettings, PreferenceFileError> {
    match std::fs::read(dir.join(CLIENT_SETTINGS_FILE)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| PreferenceFileError::Malformed(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ClientSettings::default()),
        Err(e) => Err(e.into()),
    }
}

/// Read the settings in `dir`, change them with `edit`, and write them back
/// atomically. Returns what was written.
///
/// # Errors
///
/// As [`read_client_settings`], and [`PreferenceFileError::Io`] for a file
/// that cannot be written.
pub fn update_client_settings(
    dir: &Path,
    edit: impl FnOnce(&mut ClientSettings),
) -> Result<ClientSettings, PreferenceFileError> {
    let mut settings = read_client_settings(dir)?;
    edit(&mut settings);
    let body =
        serde_json::to_vec(&settings).map_err(|e| PreferenceFileError::Malformed(e.to_string()))?;
    write_atomically(dir, CLIENT_SETTINGS_FILE, &body)?;
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work() -> FocusStream {
        FocusStream {
            stream: EntityRef::new(EntityKind::Stream, [1; 16]).to_string(),
            name: "Work".into(),
        }
    }

    /// Device-local and persisted: iOS runs the filter's intent when the
    /// Focus changes, possibly long before anybody opens Sunrise.
    #[test]
    fn the_focus_filter_survives_a_relaunch_and_is_lifted_for_good() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_client_settings(dir.path()).unwrap().focus_scope(),
            None
        );

        update_client_settings(dir.path(), |s| {
            s.set_focus_filter(vec![
                work(),
                FocusStream {
                    name: "Again".into(),
                    ..work()
                },
            ]);
        })
        .unwrap();
        let read = read_client_settings(dir.path()).unwrap();
        assert_eq!(
            read.focus_filter,
            [work()],
            "a stream picked twice keeps its first name"
        );
        assert_eq!(
            read.focus_scope(),
            Some(BTreeSet::from([EntityRef::new(
                EntityKind::Stream,
                [1; 16]
            )]))
        );

        update_client_settings(dir.path(), |s| s.set_focus_filter(Vec::new())).unwrap();
        assert_eq!(
            read_client_settings(dir.path()).unwrap().focus_scope(),
            None
        );
        assert!(!dir.path().join("client-settings.json.tmp").exists());
    }

    #[test]
    fn a_blank_account_address_is_no_address() {
        let mut s = ClientSettings::default();
        s.set_account_email(Some("  me@example.com "));
        assert_eq!(s.account_email.as_deref(), Some("me@example.com"));
        s.set_account_email(Some("   "));
        assert_eq!(s.account_email, None);
    }

    /// A newer build's field survives an older build's write.
    #[test]
    fn an_unknown_field_is_written_back_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CLIENT_SETTINGS_FILE),
            br#"{"keyboard_tips":true,"future":{"x":1}}"#,
        )
        .unwrap();
        update_client_settings(dir.path(), |s| s.set_account_email(Some("a@b.c"))).unwrap();
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(CLIENT_SETTINGS_FILE)).unwrap())
                .unwrap();
        assert_eq!(raw["future"]["x"], 1);
        assert_eq!(raw["keyboard_tips"], true);
        assert_eq!(raw["account_email"], "a@b.c");
    }

    #[test]
    fn a_file_that_is_not_settings_is_an_error_not_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CLIENT_SETTINGS_FILE), b"[1,2]").unwrap();
        let err = read_client_settings(dir.path()).unwrap_err();
        assert!(matches!(err, PreferenceFileError::Malformed(_)), "{err}");
    }
}

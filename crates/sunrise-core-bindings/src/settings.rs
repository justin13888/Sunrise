//! Device-local settings across the seam: the client settings file, the
//! Focus filter's rules, and the one-time move off the Apple client's
//! `UserDefaults` — [`sunrise_client_core::preferences::client`],
//! [`sunrise_client_core::focus`] and
//! [`sunrise_client_core::preferences::legacy`], mirrored.
//!
//! The settings with a key in the core's key table — the notification
//! switches, vim mode, the attachment cache — are not here: they are read
//! with `CoreQuery::Preferences` and written with `CoreCommand::SetPreference`
//! like every other preference.

use std::collections::BTreeMap;
use std::path::Path;

use sunrise_client_core::focus;
use sunrise_client_core::preferences::{self, legacy, ClientSettings, FocusStream};
use sunrise_id::EntityRef;

use crate::dto::TaskItem;
use crate::{BindingError, SunriseCore};

/// One stream a Focus filter picked, with the name it was picked under.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FocusFilterStream {
    /// The stream.
    pub stream: EntityRef,
    /// Its name when picked, so the filter can be described without a vault.
    pub name: String,
}

impl From<FocusFilterStream> for FocusStream {
    fn from(f: FocusFilterStream) -> Self {
        Self {
            stream: f.stream.to_string(),
            name: f.name,
        }
    }
}

/// See [`sunrise_client_core::preferences::ClientSettings`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ClientSettingsItem {
    /// The Focus filter in force; empty is no filter.
    pub focus_filter: Vec<FocusFilterStream>,
    /// The address an account is registered under.
    pub account_email: Option<String>,
    /// Whether keyboard tips are shown; absent reads as off.
    pub keyboard_tips: Option<bool>,
}

impl From<ClientSettings> for ClientSettingsItem {
    fn from(s: ClientSettings) -> Self {
        Self {
            focus_filter: s
                .focus_filter
                .iter()
                .filter_map(|f| {
                    EntityRef::parse(&f.stream, sunrise_id::EntityKind::Stream)
                        .ok()
                        .map(|stream| FocusFilterStream {
                            stream,
                            name: f.name.clone(),
                        })
                })
                .collect(),
            account_email: s.account_email,
            keyboard_tips: s.keyboard_tips,
        }
    }
}

fn file_error(e: preferences::PreferenceFileError) -> BindingError {
    BindingError::Core(e.to_string())
}

fn update(
    dir: &str,
    edit: impl FnOnce(&mut ClientSettings),
) -> Result<ClientSettingsItem, BindingError> {
    preferences::update_client_settings(Path::new(dir), edit)
        .map(Into::into)
        .map_err(file_error)
}

/// The client settings in `dir`, the directory the client keeps its vaults
/// and the bootstrap file in. The defaults when there is no file.
///
/// # Errors
///
/// A file that exists and cannot be read, or is not settings.
#[uniffi::export]
pub fn client_settings(dir: String) -> Result<ClientSettingsItem, BindingError> {
    preferences::read_client_settings(Path::new(&dir))
        .map(Into::into)
        .map_err(file_error)
}

/// Put a Focus filter in force, or lift it with an empty list. Returns the
/// settings as written.
///
/// # Errors
///
/// As [`client_settings`], or a file that cannot be written.
#[uniffi::export]
pub fn set_focus_filter(
    dir: String,
    streams: Vec<FocusFilterStream>,
) -> Result<ClientSettingsItem, BindingError> {
    update(&dir, |s| {
        s.set_focus_filter(streams.into_iter().map(Into::into).collect());
    })
}

/// Set the account address, trimmed; blank or absent clears it.
///
/// # Errors
///
/// As [`set_focus_filter`].
#[uniffi::export]
pub fn set_account_email(
    dir: String,
    email: Option<String>,
) -> Result<ClientSettingsItem, BindingError> {
    update(&dir, |s| s.set_account_email(email.as_deref()))
}

/// Show or hide keyboard tips; absent forgets the choice.
///
/// # Errors
///
/// As [`set_focus_filter`].
#[uniffi::export]
pub fn set_keyboard_tips(
    dir: String,
    shown: Option<bool>,
) -> Result<ClientSettingsItem, BindingError> {
    update(&dir, |s| s.keyboard_tips = shown)
}

/// Whether a row filed in `stream` is shown, or its reminder fires, under a
/// Focus filter picking `scope` (empty: no filter). An unknown stream is let
/// through. See [`sunrise_client_core::focus::allows`].
#[uniffi::export]
#[must_use]
pub fn focus_filter_allows(stream: Option<EntityRef>, scope: Vec<EntityRef>) -> bool {
    focus::allows(stream, focus::scope(scope).as_ref())
}

/// Today's rows narrowed to `scope` (empty: no filter), in order.
#[uniffi::export]
#[must_use]
pub fn focus_filter_scoping(tasks: Vec<TaskItem>, scope: Vec<EntityRef>) -> Vec<TaskItem> {
    focus::scoping_by(tasks, focus::scope(scope).as_ref(), |t| Some(t.stream_id))
}

/// The line a Focus filter is described by. See
/// [`sunrise_client_core::focus::summary`].
#[uniffi::export]
#[must_use]
pub fn focus_filter_summary(names: Vec<String>) -> String {
    focus::summary(&names)
}

/// One `UserDefaults` value, as the Apple client held it.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum LegacyDefaultValue {
    /// A `Bool`.
    Bool {
        /// The value.
        value: bool,
    },
    /// An `Int`.
    Int {
        /// The value.
        value: i64,
    },
    /// A `String`.
    Text {
        /// The value.
        value: String,
    },
    /// The Focus filter's `[id: name]` dictionary.
    Streams {
        /// The streams.
        streams: Vec<FocusFilterStream>,
    },
}

/// One `UserDefaults` key and its value.
#[derive(Debug, Clone, uniffi::Record)]
pub struct LegacyDefault {
    /// The old key, e.g. `notify.leadMinutes`.
    pub key: String,
    /// Its value.
    pub value: LegacyDefaultValue,
}

/// What [`SunriseCore::migrate_legacy_defaults`] did.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LegacyMigrationReport {
    /// Core preferences written.
    pub preferences_written: u32,
    /// Bootstrap preferences written.
    pub bootstrap_written: u32,
    /// The old switch said this device is primary: claim the register
    /// ([`SunriseCore::claim_primary_reminder_device`]) while it is unset.
    pub claim_primary: bool,
    /// Keys that map nowhere; leave them in `UserDefaults`.
    pub unmapped: Vec<String>,
}

#[uniffi::export(async_runtime = "tokio")]
impl SunriseCore {
    /// Move the Apple client's old `UserDefaults` settings to where they live
    /// now: core preferences into this vault, bootstrap keys and client
    /// settings into `dir`. Only values the user changed move. Idempotent;
    /// remove the old keys once it returns. See
    /// [`sunrise_client_core::preferences::legacy`].
    pub async fn migrate_legacy_defaults(
        &self,
        dir: String,
        defaults: Vec<LegacyDefault>,
    ) -> Result<LegacyMigrationReport, BindingError> {
        let old: BTreeMap<String, legacy::LegacyValue> = defaults
            .into_iter()
            .map(|d| {
                let value = match d.value {
                    LegacyDefaultValue::Bool { value } => legacy::LegacyValue::Bool(value),
                    LegacyDefaultValue::Int { value } => legacy::LegacyValue::Int(value),
                    LegacyDefaultValue::Text { value } => legacy::LegacyValue::Text(value),
                    LegacyDefaultValue::Streams { streams } => {
                        legacy::LegacyValue::Streams(streams.into_iter().map(Into::into).collect())
                    }
                };
                (d.key, value)
            })
            .collect();
        let m = legacy::migrate(&old);
        legacy::apply(&self.inner, Path::new(&dir), &m)
            .await
            .map_err(|e| match e {
                legacy::ApplyError::Core(c) => BindingError::from(c),
                legacy::ApplyError::File(f) => file_error(f),
            })?;
        Ok(LegacyMigrationReport {
            preferences_written: u32::try_from(m.preferences.len()).unwrap_or(u32::MAX),
            bootstrap_written: u32::try_from(m.bootstrap.len()).unwrap_or(u32::MAX),
            claim_primary: m.claim_primary,
            unmapped: m.unmapped,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_id::EntityKind;

    fn work() -> FocusFilterStream {
        FocusFilterStream {
            stream: EntityRef::new(EntityKind::Stream, [1; 16]),
            name: "Work".into(),
        }
    }

    #[test]
    fn the_focus_filter_round_trips_the_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let at = dir.path().to_str().unwrap().to_owned();
        assert!(client_settings(at.clone()).unwrap().focus_filter.is_empty());
        let written = set_focus_filter(at.clone(), vec![work()]).unwrap();
        assert_eq!(written.focus_filter, [work()]);
        assert_eq!(client_settings(at.clone()).unwrap(), written);
        assert!(set_focus_filter(at.clone(), vec![])
            .unwrap()
            .focus_filter
            .is_empty());
        assert_eq!(
            set_account_email(at.clone(), Some(" a@b.c ".into()))
                .unwrap()
                .account_email
                .as_deref(),
            Some("a@b.c")
        );
        assert_eq!(
            set_keyboard_tips(at, Some(true)).unwrap().keyboard_tips,
            Some(true)
        );
    }

    #[test]
    fn the_focus_rules_cross_as_the_client_layer_states_them() {
        let work_id = work().stream;
        let home = EntityRef::new(EntityKind::Stream, [2; 16]);
        assert!(focus_filter_allows(Some(work_id), vec![work_id]));
        assert!(!focus_filter_allows(Some(home), vec![work_id]));
        assert!(
            focus_filter_allows(Some(home), vec![]),
            "no pick is no filter"
        );
        assert!(focus_filter_allows(None, vec![work_id]));
        assert_eq!(focus_filter_summary(vec![]), "All streams");
    }
}

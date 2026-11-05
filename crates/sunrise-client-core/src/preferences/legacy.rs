//! The Apple client's `UserDefaults` settings, mapped onto where they live now
//! (#489).
//!
//! Before the client layer owned them, the Apple app kept its notification
//! settings, its relay and sign-in settings, its editor and attachment
//! switches and its Focus filter in `UserDefaults`, under keys of its own.
//! Every one of those now has a home that is not the app's: a key in the
//! core's key table, a bootstrap key, or a client setting
//! ([`super::client`]). [`migrate`] reads the old keys once and says what to
//! write where; [`apply`] writes it.
//!
//! **A value equal to the old default is not carried over.** The notification
//! keys are `vault_overridable`, and writing this device's overlay pins the
//! key here even when the user never touched it — the account's quiet hours
//! would then never reach this device. Only what the user changed moves.

use std::collections::BTreeMap;
use std::path::Path;

use jiff::civil;
use sunrise_core::{Command, Core, CoreError};
use sunrise_domain::{PrefTarget, PrefValue};

use super::client::FocusStream;
use super::{set_bootstrap_preference, update_client_settings, PreferenceFileError};

/// One value as `UserDefaults` held it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyValue {
    /// A `Bool`.
    Bool(bool),
    /// An `Int`.
    Int(i64),
    /// A `String`.
    Text(String),
    /// The Focus filter's `[id: name]` dictionary.
    Streams(Vec<FocusStream>),
}

/// What the old settings become.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Migration {
    /// Core preferences to set: key, value, store.
    pub preferences: Vec<(&'static str, PrefValue, PrefTarget)>,
    /// Bootstrap preferences to set: key, value.
    pub bootstrap: Vec<(&'static str, String)>,
    /// The Focus filter, when one was in force.
    pub focus_filter: Option<Vec<FocusStream>>,
    /// The account address, when one was set.
    pub account_email: Option<String>,
    /// Whether keyboard tips were turned on.
    pub keyboard_tips: Option<bool>,
    /// The old switch said this device is the primary reminder device. The
    /// register is account-wide, so the caller claims it only while it is
    /// unset ([`crate::reminders::ReminderPolicy::claim_primary`]); `false`
    /// carries nothing, since another device claims it.
    pub claim_primary: bool,
    /// Keys handed in that map nowhere, so the caller can leave them alone.
    pub unmapped: Vec<String>,
}

/// The old minutes-past-midnight, as a civil time.
fn clock(minutes: i64) -> civil::Time {
    let m = minutes.rem_euclid(1440);
    // In range by construction: 0..=23 hours, 0..=59 minutes.
    civil::Time::new(
        i8::try_from(m / 60).unwrap_or(0),
        i8::try_from(m % 60).unwrap_or(0),
        0,
        0,
    )
    .unwrap_or(civil::Time::midnight())
}

fn text(value: Option<&LegacyValue>) -> Option<String> {
    match value {
        Some(LegacyValue::Text(t)) if !t.trim().is_empty() => Some(t.trim().to_owned()),
        _ => None,
    }
}

fn flag(value: Option<&LegacyValue>) -> Option<bool> {
    match value {
        Some(LegacyValue::Bool(b)) => Some(*b),
        _ => None,
    }
}

fn int(value: Option<&LegacyValue>) -> Option<i64> {
    match value {
        Some(LegacyValue::Int(i)) => Some(*i),
        _ => None,
    }
}

/// Every old key this module knows.
const KNOWN: &[&str] = &[
    "notify.enabled",
    "notify.isPrimaryDevice",
    "notify.leadMinutes",
    "notify.quietHours.enabled",
    "notify.quietHours.startMinutes",
    "notify.quietHours.endMinutes",
    "notify.quietHours.drop",
    "sync.relayURL",
    "auth.oidcIssuer",
    "auth.oidcClientID",
    "account.email",
    "editor.vim_mode",
    "keyboard.tips",
    "attachments.cache_limit_bytes",
    "attachments.auto_fetch_on_cellular",
    "focus.filter.streams",
];

/// What the old settings in `old` become.
#[must_use]
pub fn migrate(old: &BTreeMap<String, LegacyValue>) -> Migration {
    let get = |k: &str| old.get(k);
    let mut m = Migration::default();
    let mut device = |key: &'static str, value: PrefValue| {
        m.preferences.push((key, value, PrefTarget::Device));
    };

    if flag(get("notify.enabled")) == Some(false) {
        device("notifications.enabled", PrefValue::Bool(false));
    }
    if let Some(minutes) = int(get("notify.leadMinutes")).filter(|m| *m > 0) {
        device(
            "notifications.task_reminder.lead_s",
            PrefValue::Uint(u64::try_from(minutes.saturating_mul(60)).unwrap_or(0)),
        );
    }
    if flag(get("notify.quietHours.enabled")) == Some(true) {
        let start = clock(int(get("notify.quietHours.startMinutes")).unwrap_or(22 * 60));
        let end = clock(int(get("notify.quietHours.endMinutes")).unwrap_or(7 * 60));
        // Equal ends silenced nothing there and are refused by the key here.
        if start != end {
            device(
                "notifications.quiet_hours.window",
                PrefValue::TimeWindow { start, end },
            );
        }
        if flag(get("notify.quietHours.drop")) == Some(true) {
            device(
                "notifications.quiet_hours.policy",
                PrefValue::Text("drop".into()),
            );
        }
    }
    if flag(get("editor.vim_mode")) == Some(true) {
        device("keyboard.vim_mode", PrefValue::Bool(true));
    }
    if let Some(bytes) = int(get("attachments.cache_limit_bytes")).filter(|b| *b > 0) {
        device(
            "attachments.cache_limit_bytes",
            PrefValue::Uint(u64::try_from(bytes).unwrap_or(0)),
        );
    }
    if flag(get("attachments.auto_fetch_on_cellular")) == Some(true) {
        device("attachments.auto_fetch_on_cellular", PrefValue::Bool(true));
    }

    for (old_key, key) in [
        ("sync.relayURL", "sync.relay_url"),
        ("auth.oidcIssuer", "auth.oidc_issuer"),
        ("auth.oidcClientID", "auth.oidc_client_id"),
    ] {
        if let Some(v) = text(get(old_key)) {
            m.bootstrap.push((key, v));
        }
    }
    m.account_email = text(get("account.email"));
    m.keyboard_tips = flag(get("keyboard.tips")).filter(|on| *on);
    if let Some(LegacyValue::Streams(s)) = get("focus.filter.streams") {
        m.focus_filter = (!s.is_empty()).then(|| s.clone());
    }
    m.claim_primary = flag(get("notify.isPrimaryDevice")).unwrap_or(true);
    m.unmapped = old
        .keys()
        .filter(|k| !KNOWN.contains(&k.as_str()))
        .cloned()
        .collect();
    m
}

/// Why [`apply`] stopped.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// A core preference write was refused.
    #[error(transparent)]
    Core(#[from] CoreError),
    /// A file in `dir` could not be written.
    #[error(transparent)]
    File(#[from] PreferenceFileError),
}

/// Write `migration`: core preferences through `core`, bootstrap keys and
/// client settings into `dir`, the directory the client keeps both files in.
///
/// Idempotent: run twice, it writes the same values twice. The caller drops
/// the old keys once it returns `Ok`, and claims the primary register itself
/// ([`Migration::claim_primary`]).
///
/// # Errors
///
/// The first write that fails; the ones before it stay written.
pub async fn apply(core: &Core, dir: &Path, migration: &Migration) -> Result<(), ApplyError> {
    for (key, value, target) in &migration.preferences {
        core.submit(Command::SetPreference {
            key: (*key).to_owned(),
            value: value.clone(),
            target: *target,
        })
        .await?;
    }
    for (key, value) in &migration.bootstrap {
        set_bootstrap_preference(dir, key, Some(value))?;
    }
    if migration.focus_filter.is_some()
        || migration.account_email.is_some()
        || migration.keyboard_tips.is_some()
    {
        update_client_settings(dir, |s| {
            if let Some(f) = &migration.focus_filter {
                s.set_focus_filter(f.clone());
            }
            if let Some(e) = &migration.account_email {
                s.set_account_email(Some(e));
            }
            if migration.keyboard_tips.is_some() {
                s.keyboard_tips = migration.keyboard_tips;
            }
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn old(entries: &[(&str, LegacyValue)]) -> BTreeMap<String, LegacyValue> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    /// Untouched settings move nowhere: pinning the old defaults as this
    /// device's overlay would hide the account's own values for good.
    #[test]
    fn defaults_are_not_carried_over() {
        let m = migrate(&old(&[
            ("notify.enabled", LegacyValue::Bool(true)),
            ("notify.leadMinutes", LegacyValue::Int(0)),
            ("notify.quietHours.enabled", LegacyValue::Bool(false)),
            ("notify.quietHours.drop", LegacyValue::Bool(true)),
            ("sync.relayURL", LegacyValue::Text("  ".into())),
        ]));
        assert!(m.preferences.is_empty(), "{:?}", m.preferences);
        assert!(m.bootstrap.is_empty());
        assert!(m.claim_primary, "the old default was primary");
    }

    #[test]
    fn changed_notification_settings_land_on_their_keys() {
        let m = migrate(&old(&[
            ("notify.enabled", LegacyValue::Bool(false)),
            ("notify.isPrimaryDevice", LegacyValue::Bool(false)),
            ("notify.leadMinutes", LegacyValue::Int(15)),
            ("notify.quietHours.enabled", LegacyValue::Bool(true)),
            (
                "notify.quietHours.startMinutes",
                LegacyValue::Int(22 * 60 + 30),
            ),
            ("notify.quietHours.drop", LegacyValue::Bool(true)),
        ]));
        let keys: Vec<_> = m
            .preferences
            .iter()
            .map(|(k, v, _)| (*k, v.clone()))
            .collect();
        assert_eq!(
            keys,
            [
                ("notifications.enabled", PrefValue::Bool(false)),
                ("notifications.task_reminder.lead_s", PrefValue::Uint(900)),
                (
                    "notifications.quiet_hours.window",
                    PrefValue::TimeWindow {
                        start: civil::time(22, 30, 0, 0),
                        end: civil::time(7, 0, 0, 0),
                    }
                ),
                (
                    "notifications.quiet_hours.policy",
                    PrefValue::Text("drop".into())
                ),
            ]
        );
        assert!(m
            .preferences
            .iter()
            .all(|(_, _, t)| *t == PrefTarget::Device));
        assert!(!m.claim_primary);
        for (key, value, target) in &m.preferences {
            let spec = sunrise_domain::pref_spec(key).expect("a key in the table");
            assert!(spec.scope.permits(*target), "{key}");
            assert!(spec.ty.fits(value), "{key}");
        }
    }

    /// A zero-width window silenced nothing there, and the key refuses one.
    #[test]
    fn a_zero_width_quiet_window_is_not_carried_over() {
        let m = migrate(&old(&[
            ("notify.quietHours.enabled", LegacyValue::Bool(true)),
            ("notify.quietHours.startMinutes", LegacyValue::Int(60)),
            ("notify.quietHours.endMinutes", LegacyValue::Int(60)),
        ]));
        assert!(m.preferences.is_empty());
    }

    #[test]
    fn connection_editor_and_focus_settings_find_their_homes() {
        let focus = vec![FocusStream {
            stream: "str_x".into(),
            name: "Work".into(),
        }];
        let m = migrate(&old(&[
            (
                "sync.relayURL",
                LegacyValue::Text("wss://relay.example/sync".into()),
            ),
            ("auth.oidcClientID", LegacyValue::Text("sunrise".into())),
            (
                "account.email",
                LegacyValue::Text(" me@example.com ".into()),
            ),
            ("editor.vim_mode", LegacyValue::Bool(true)),
            ("keyboard.tips", LegacyValue::Bool(true)),
            ("focus.filter.streams", LegacyValue::Streams(focus.clone())),
            ("vaults.selected", LegacyValue::Text("a".into())),
        ]));
        assert_eq!(
            m.bootstrap,
            [
                ("sync.relay_url", "wss://relay.example/sync".to_owned()),
                ("auth.oidc_client_id", "sunrise".to_owned()),
            ]
        );
        assert_eq!(m.account_email.as_deref(), Some("me@example.com"));
        assert_eq!(m.keyboard_tips, Some(true));
        assert_eq!(m.focus_filter, Some(focus));
        assert_eq!(
            m.preferences,
            [(
                "keyboard.vim_mode",
                PrefValue::Bool(true),
                PrefTarget::Device
            )]
        );
        assert_eq!(
            m.unmapped,
            ["vaults.selected"],
            "an OS-specific key stays put"
        );
    }

    #[test]
    fn the_old_clock_wraps_like_the_old_picker() {
        assert_eq!(clock(0), civil::time(0, 0, 0, 0));
        assert_eq!(clock(1439), civil::time(23, 59, 0, 0));
        assert_eq!(clock(1440 + 61), civil::time(1, 1, 0, 0));
        assert_eq!(clock(-60), civil::time(23, 0, 0, 0));
    }
}

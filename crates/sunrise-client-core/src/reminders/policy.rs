//! The notification preferences, read off the core's `Preferences`
//! (`docs/08-features/notifications.md` §Preference keys, #489).
//!
//! The Apple client used to keep its own copy of these in `UserDefaults` —
//! a master switch, a primary-device flag, a lead time and a quiet window —
//! and hand the core a `ReminderSettings` built from it. Every one of them is
//! a key in the core's key table, resolved there (overlay, then vault, then
//! default), so this module reads them from [`sunrise_core::Query::Preferences`]
//! and nothing is kept beside the vault.

use sunrise_domain::{
    PrefTarget, PrefValue, QuietHours, QuietHoursPolicy, ReminderKind, ReminderSettings,
    ResolvedPref,
};
use sunrise_id::{EntityKind, EntityRef};

/// The register naming the account's primary reminder device.
pub const PRIMARY_DEVICE_KEY: &str = "notifications.primary_device";

/// What this device's reminders are allowed to do, from its preferences.
#[derive(Debug, Clone)]
// Each flag is one independent `.enabled` key of the table, not a state
// machine folded into booleans.
#[allow(clippy::struct_excessive_bools)]
pub struct ReminderPolicy {
    /// `notifications.enabled`: the master switch on this install. Off means
    /// nothing is handed to the OS, and anything held is withdrawn.
    pub enabled: bool,
    /// What the core's planner takes.
    pub settings: ReminderSettings,
    /// `notifications.task_reminder.enabled`.
    pub task_enabled: bool,
    /// `notifications.block_start.enabled`.
    pub block_enabled: bool,
    /// `notifications.routine_due.enabled`.
    pub routine_enabled: bool,
    /// `notifications.wind_down.enabled`: fire the wind-down
    /// ([`super::wind_down_notification`]) on this device when it is the
    /// primary one and the night has a sleep time.
    pub wind_down_enabled: bool,
    /// `notifications.wind_down.lead_s`: how long before the sleep time.
    pub wind_down_lead_s: u32,
    /// Whether this device should claim the primary register: it is unset (or
    /// names a revoked device) and notifications are on here. Write
    /// [`primary_claim`] when it is.
    pub claim_primary: bool,
}

impl ReminderPolicy {
    /// Whether reminders of `kind` are on.
    #[must_use]
    pub const fn kind_enabled(&self, kind: ReminderKind) -> bool {
        match kind {
            ReminderKind::Task => self.task_enabled,
            ReminderKind::Block => self.block_enabled,
            ReminderKind::Routine => self.routine_enabled,
        }
    }
}

/// The value `key` resolves to: from `prefs` when it is there, else the key
/// table's default.
pub(super) fn value(prefs: &[ResolvedPref], key: &str) -> Option<PrefValue> {
    match prefs.iter().find(|p| p.key == key) {
        Some(p) => p.value.clone(),
        None => sunrise_domain::pref_spec(key).and_then(|spec| {
            sunrise_domain::resolve_preference(
                spec,
                None,
                None,
                sunrise_domain::DeviceClass::Desktop,
            )
            .value
        }),
    }
}

pub(super) fn flag(prefs: &[ResolvedPref], key: &str, default: bool) -> bool {
    match value(prefs, key) {
        Some(PrefValue::Bool(b)) => b,
        _ => default,
    }
}

/// The policy `prefs` describe for the device `this_device`, with `revoked`
/// the devices the account currently calls revoked.
///
/// **Primary device.** `notifications.primary_device` naming this device, or
/// naming nothing — unset, or a revoked device, which the spec reads as unset
/// (§Multi-device dedup) — makes this device primary. Unset reads as primary
/// rather than silent because a fresh install is usually somebody's only
/// device, and a reading that silenced it would ship reminders that never
/// fire; [`ReminderPolicy::claim_primary`] then asks the client to write the
/// claim, so a second device reads it as taken.
///
/// **Quiet hours.** `notifications.quiet_hours.window` absent means never
/// quiet. The key table refuses a window whose start equals its end, so one
/// never reaches the planner.
#[must_use]
pub fn reminder_policy(
    prefs: &[ResolvedPref],
    this_device: EntityRef,
    revoked: &[EntityRef],
) -> ReminderPolicy {
    let enabled = flag(prefs, "notifications.enabled", true);
    let primary = match value(prefs, PRIMARY_DEVICE_KEY) {
        Some(PrefValue::Text(s)) => EntityRef::parse(&s, EntityKind::Device)
            .ok()
            .filter(|d| !revoked.contains(d)),
        _ => None,
    };
    let is_primary_device = primary.is_none_or(|d| d == this_device);
    let default_lead_s = match value(prefs, "notifications.task_reminder.lead_s") {
        Some(PrefValue::Uint(s)) => u32::try_from(s).unwrap_or(u32::MAX),
        _ => 0,
    };
    let policy = match value(prefs, "notifications.quiet_hours.policy") {
        Some(PrefValue::Text(p)) if p == "drop" => QuietHoursPolicy::Drop,
        _ => QuietHoursPolicy::Queue,
    };
    let quiet_hours = match value(prefs, "notifications.quiet_hours.window") {
        Some(PrefValue::TimeWindow { start, end }) if start != end => {
            Some(QuietHours { start, end, policy })
        }
        _ => None,
    };
    ReminderPolicy {
        enabled,
        settings: ReminderSettings {
            default_lead_s,
            quiet_hours,
            is_primary_device,
        },
        task_enabled: flag(prefs, "notifications.task_reminder.enabled", true),
        block_enabled: flag(prefs, "notifications.block_start.enabled", true),
        routine_enabled: flag(prefs, "notifications.routine_due.enabled", true),
        wind_down_enabled: flag(prefs, "notifications.wind_down.enabled", true),
        wind_down_lead_s: match value(prefs, "notifications.wind_down.lead_s") {
            Some(PrefValue::Uint(s)) => u32::try_from(s).unwrap_or(u32::MAX),
            _ => 3600,
        },
        claim_primary: enabled && primary.is_none(),
    }
}

/// The write that makes `this_device` the primary reminder device: the key,
/// its value, and the vault as its target (the register is `vault`-scoped, so
/// every device reads the claim).
#[must_use]
pub fn primary_claim(this_device: EntityRef) -> (&'static str, PrefValue, PrefTarget) {
    (
        PRIMARY_DEVICE_KEY,
        PrefValue::Text(this_device.to_string()),
        PrefTarget::Vault,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_domain::{PrefScope, PrefSource};

    fn dev(n: u8) -> EntityRef {
        EntityRef::new(EntityKind::Device, [n; 16])
    }

    fn set(key: &'static str, value: PrefValue) -> ResolvedPref {
        ResolvedPref {
            key,
            scope: PrefScope::Device,
            value: Some(value),
            source: PrefSource::Overlay,
            vault_set: false,
        }
    }

    fn time(s: &str) -> jiff::civil::Time {
        s.parse().unwrap()
    }

    /// The Swift `NotificationPreferences` defaults, read off the key table:
    /// on, primary, fire at the scheduled time, never quiet.
    #[test]
    fn a_fresh_install_is_on_primary_and_fires_at_the_scheduled_time() {
        let p = reminder_policy(&[], dev(1), &[]);
        assert!(p.enabled);
        assert!(p.settings.is_primary_device);
        assert_eq!(p.settings.default_lead_s, 0, "the spec's default lead time");
        assert!(p.settings.quiet_hours.is_none(), "no window is never quiet");
        assert!(p.task_enabled && p.block_enabled && p.routine_enabled);
        assert!(p.claim_primary, "an unset register is claimed");
    }

    #[test]
    fn the_quiet_window_and_its_policy_are_carried_through() {
        let prefs = [
            set(
                "notifications.quiet_hours.window",
                PrefValue::TimeWindow {
                    start: time("22:30"),
                    end: time("07:00"),
                },
            ),
            set(
                "notifications.quiet_hours.policy",
                PrefValue::Text("drop".into()),
            ),
        ];
        let q = reminder_policy(&prefs, dev(1), &[])
            .settings
            .quiet_hours
            .unwrap();
        assert_eq!((q.start, q.end), (time("22:30"), time("07:00")));
        assert_eq!(q.policy, QuietHoursPolicy::Drop);

        let queue = reminder_policy(&prefs[..1], dev(1), &[]);
        assert_eq!(
            queue.settings.quiet_hours.unwrap().policy,
            QuietHoursPolicy::Queue,
            "the spec's default policy is queue"
        );
    }

    #[test]
    fn the_lead_time_reaches_the_planner_in_seconds() {
        let prefs = [set(
            "notifications.task_reminder.lead_s",
            PrefValue::Uint(900),
        )];
        assert_eq!(
            reminder_policy(&prefs, dev(1), &[]).settings.default_lead_s,
            900
        );
    }

    /// One register, one primary: the device it names is primary, every other
    /// stays silent, and a revoked one reads as unset.
    #[test]
    fn only_the_device_the_register_names_is_primary() {
        let prefs = [set(PRIMARY_DEVICE_KEY, PrefValue::Text(dev(1).to_string()))];
        assert!(
            reminder_policy(&prefs, dev(1), &[])
                .settings
                .is_primary_device
        );
        let other = reminder_policy(&prefs, dev(2), &[]);
        assert!(!other.settings.is_primary_device);
        assert!(!other.claim_primary, "a held register is not claimed");

        let after_revoke = reminder_policy(&prefs, dev(2), &[dev(1)]);
        assert!(after_revoke.settings.is_primary_device);
        assert!(after_revoke.claim_primary);
    }

    #[test]
    fn the_master_switch_and_each_kind_are_read() {
        let prefs = [
            set("notifications.enabled", PrefValue::Bool(false)),
            set("notifications.block_start.enabled", PrefValue::Bool(false)),
        ];
        let p = reminder_policy(&prefs, dev(1), &[]);
        assert!(!p.enabled);
        assert!(
            !p.claim_primary,
            "a device with reminders off claims nothing"
        );
        assert!(p.kind_enabled(ReminderKind::Task));
        assert!(!p.kind_enabled(ReminderKind::Block));
        assert!(p.kind_enabled(ReminderKind::Routine));
    }

    #[test]
    fn the_claim_writes_this_device_to_the_vault() {
        let (key, value, target) = primary_claim(dev(3));
        assert_eq!(key, PRIMARY_DEVICE_KEY);
        assert_eq!(target, PrefTarget::Vault);
        let PrefValue::Text(text) = value else {
            panic!("a device ref is text")
        };
        assert_eq!(EntityRef::parse(&text, EntityKind::Device).unwrap(), dev(3));
        assert!(sunrise_domain::pref_spec(key).is_some());
    }
}

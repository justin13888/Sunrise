//! The key table: every preference key this build knows.
//!
//! Normative in `docs/02-domain/preferences.md` and
//! `docs/08-features/notifications.md` §Preference keys; a test in this
//! module's parent holds the keys and scopes equal to those pages.

use super::{PrefDefault, PrefScope, PrefSpec, PrefType};
use sunrise_id::EntityKind;

const fn key(key: &'static str, ty: PrefType, default: PrefDefault, scope: PrefScope) -> PrefSpec {
    PrefSpec {
        key,
        ty,
        default,
        scope,
        bootstrap: false,
    }
}

const fn bootstrap(key: &'static str, ty: PrefType) -> PrefSpec {
    PrefSpec {
        key,
        ty,
        default: PrefDefault::Absent,
        scope: PrefScope::Device,
        bootstrap: true,
    }
}

const fn enabled(kind: &'static str, on: bool, scope: PrefScope) -> PrefSpec {
    key(kind, PrefType::Bool, PrefDefault::Bool(on), scope)
}

/// Any `u32`: lead times reach the reminder planner as one.
const LEAD_S: PrefType = PrefType::Uint {
    min: 0,
    max: u32::MAX as u64,
};

const OFFSET_S: PrefType = PrefType::Int {
    min: -86_400,
    max: 86_400,
};

use PrefScope::{Device, Vault, VaultOverridable};

/// Every key this build knows, in the order `preferences.md` and
/// `notifications.md` §Preference keys list them. Normative there; a test
/// holds the two in step.
pub static PREFERENCE_KEYS: &[PrefSpec] = &[
    // Calendar and time.
    key(
        "week_start",
        PrefType::Weekday,
        PrefDefault::Weekday("SU"),
        Vault,
    ),
    key(
        "home_timezone",
        PrefType::TimeZone,
        PrefDefault::Absent,
        Vault,
    ),
    key(
        "time_format",
        PrefType::OneOf(&["locale", "h12", "h24"]),
        PrefDefault::Text("locale"),
        VaultOverridable,
    ),
    // Tasks, triage and reviews.
    key(
        "stale_after_days",
        PrefType::Uint { min: 0, max: 365 },
        PrefDefault::Uint(14),
        Vault,
    ),
    key(
        "capture_default_stream",
        PrefType::Ref(EntityKind::Stream),
        PrefDefault::Absent,
        Vault,
    ),
    key(
        "review.cadence",
        PrefType::Cadence,
        PrefDefault::Cadence("FR", "16:00:00"),
        Vault,
    ),
    // Planner and views.
    key(
        "planner.snap_s",
        PrefType::Uint { min: 60, max: 3600 },
        PrefDefault::Uint(900),
        Vault,
    ),
    key(
        "planner.default_task_duration_s",
        PrefType::Uint {
            min: 60,
            max: 86_400,
        },
        PrefDefault::Uint(1800),
        Vault,
    ),
    key(
        "planner.min_gap_s",
        PrefType::Uint { min: 0, max: 3600 },
        PrefDefault::Uint(0),
        Vault,
    ),
    key(
        "views.upcoming.span_days",
        PrefType::UintOneOf(&[7, 14, 30]),
        PrefDefault::Uint(7),
        Device,
    ),
    key(
        "search.content_language",
        PrefType::LanguageTag,
        PrefDefault::Absent,
        Vault,
    ),
    // Notifications: `docs/08-features/notifications.md` §Preference keys.
    enabled(
        "notifications.task_reminder.enabled",
        true,
        VaultOverridable,
    ),
    key(
        "notifications.task_reminder.lead_s",
        LEAD_S,
        PrefDefault::Uint(0),
        VaultOverridable,
    ),
    enabled("notifications.deadline.enabled", true, VaultOverridable),
    key(
        "notifications.deadline.lead_s",
        LEAD_S,
        PrefDefault::Uint(3600),
        Vault,
    ),
    enabled("notifications.routine_due.enabled", true, VaultOverridable),
    enabled("notifications.block_start.enabled", true, VaultOverridable),
    key(
        "notifications.block_start.lead_s",
        LEAD_S,
        PrefDefault::Uint(900),
        Vault,
    ),
    enabled(
        "notifications.morning_brief.enabled",
        true,
        VaultOverridable,
    ),
    key(
        "notifications.morning_brief.offset_s",
        OFFSET_S,
        PrefDefault::Int(900),
        Vault,
    ),
    enabled("notifications.triage_nudge.enabled", true, VaultOverridable),
    enabled("notifications.evening_plan.enabled", true, VaultOverridable),
    key(
        "notifications.evening_plan.offset_s",
        OFFSET_S,
        PrefDefault::Int(7200),
        Vault,
    ),
    enabled("notifications.wind_down.enabled", true, VaultOverridable),
    key(
        "notifications.wind_down.lead_s",
        PrefType::Uint {
            min: 0,
            max: 14_400,
        },
        PrefDefault::Uint(3600),
        Vault,
    ),
    enabled("notifications.focus_end.enabled", true, VaultOverridable),
    enabled(
        "notifications.timezone_changed.enabled",
        false,
        VaultOverridable,
    ),
    enabled(
        "notifications.weekly_review.enabled",
        true,
        VaultOverridable,
    ),
    enabled("notifications.sync_stalled.enabled", true, Device),
    enabled(
        "notifications.calendar_reconnect.enabled",
        true,
        VaultOverridable,
    ),
    enabled(
        "notifications.shared_change.enabled",
        true,
        VaultOverridable,
    ),
    enabled("notifications.place_arrival.enabled", false, Device),
    enabled("notifications.enabled", true, Device),
    key(
        "notifications.primary_device",
        PrefType::Ref(EntityKind::Device),
        PrefDefault::Absent,
        Vault,
    ),
    key(
        "notifications.quiet_hours.window",
        PrefType::TimeWindow,
        PrefDefault::Absent,
        VaultOverridable,
    ),
    key(
        "notifications.quiet_hours.policy",
        PrefType::OneOf(&["queue", "drop"]),
        PrefDefault::Text("queue"),
        VaultOverridable,
    ),
    // Device and connection.
    bootstrap("sync.relay_url", PrefType::Url),
    bootstrap("auth.oidc_issuer", PrefType::Url),
    bootstrap("auth.oidc_client_id", PrefType::Text),
    key(
        "attachments.auto_fetch_on_cellular",
        PrefType::Bool,
        PrefDefault::Bool(false),
        Device,
    ),
    key(
        "attachments.cache_limit_bytes",
        PrefType::Uint {
            min: 100_000_000,
            max: 50_000_000_000,
        },
        PrefDefault::UintByClass {
            desktop: 1_000_000_000,
            handheld: 200_000_000,
        },
        Device,
    ),
    key(
        "keyboard.vim_mode",
        PrefType::Bool,
        PrefDefault::Bool(false),
        Device,
    ),
];

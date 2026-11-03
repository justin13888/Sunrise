//! Preferences: a resolved setting, its scope and where it came from.

use sunrise_domain::{
    PrefScope, PrefSource, PrefTarget, PrefValue, Preferences, ResolvedPref, Weekday,
};
use sunrise_id::EntityRef;

/// See [`sunrise_domain::PrefValue`]. Every case has named fields so the Swift
/// cases read `.bool(value:)`, `.cadence(day:at:)`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum PreferenceValue {
    /// A boolean key.
    Bool {
        /// The value.
        value: bool,
    },
    /// An unsigned key: a count, a duration in seconds, a byte size.
    Uint {
        /// The value.
        value: u64,
    },
    /// A signed key: an offset in seconds.
    Int {
        /// The value.
        value: i64,
    },
    /// Every text-valued key: an enumerated spelling, a URL, a zone name, a
    /// language tag, an entity or device id.
    Text {
        /// The value.
        value: String,
    },
    /// A weekday.
    Weekday {
        /// The day.
        day: Weekday,
    },
    /// A day and a civil time on it (`review.cadence`).
    Cadence {
        /// The day.
        day: Weekday,
        /// The civil time.
        at: jiff::civil::Time,
    },
    /// A civil window; an `end` before `start` wraps midnight.
    TimeWindow {
        /// Civil start.
        start: jiff::civil::Time,
        /// Civil end.
        end: jiff::civil::Time,
    },
    /// A day-schedule window (`day_schedule.*` entries); a `sleep` at or
    /// before `wake` is the next morning.
    DayWindow {
        /// Civil wake time.
        wake: jiff::civil::Time,
        /// Civil sleep time.
        sleep: jiff::civil::Time,
    },
    /// A `day_schedule.date.*` override saying there is no schedule that
    /// date.
    NoSchedule,
}

impl From<PrefValue> for PreferenceValue {
    fn from(v: PrefValue) -> Self {
        match v {
            PrefValue::Bool(value) => Self::Bool { value },
            PrefValue::Uint(value) => Self::Uint { value },
            PrefValue::Int(value) => Self::Int { value },
            PrefValue::Text(value) => Self::Text { value },
            PrefValue::Weekday(day) => Self::Weekday { day },
            PrefValue::Cadence { day, at } => Self::Cadence { day, at },
            PrefValue::TimeWindow { start, end } => Self::TimeWindow { start, end },
            PrefValue::DayWindow { wake, sleep } => Self::DayWindow { wake, sleep },
            PrefValue::NoSchedule => Self::NoSchedule,
        }
    }
}

impl From<PreferenceValue> for PrefValue {
    fn from(v: PreferenceValue) -> Self {
        match v {
            PreferenceValue::Bool { value } => Self::Bool(value),
            PreferenceValue::Uint { value } => Self::Uint(value),
            PreferenceValue::Int { value } => Self::Int(value),
            PreferenceValue::Text { value } => Self::Text(value),
            PreferenceValue::Weekday { day } => Self::Weekday(day),
            PreferenceValue::Cadence { day, at } => Self::Cadence { day, at },
            PreferenceValue::TimeWindow { start, end } => Self::TimeWindow { start, end },
            PreferenceValue::DayWindow { wake, sleep } => Self::DayWindow { wake, sleep },
            PreferenceValue::NoSchedule => Self::NoSchedule,
        }
    }
}

/// See [`sunrise_domain::PrefScope`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PreferenceScope {
    /// Synced; one value for the account.
    Vault,
    /// Synced default that this device may override.
    VaultOverridable,
    /// This device only.
    Device,
}

impl From<PrefScope> for PreferenceScope {
    fn from(s: PrefScope) -> Self {
        match s {
            PrefScope::Vault => Self::Vault,
            PrefScope::VaultOverridable => Self::VaultOverridable,
            PrefScope::Device => Self::Device,
        }
    }
}

/// See [`sunrise_domain::PrefTarget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PreferenceTarget {
    /// The synced value.
    Vault,
    /// This device's override, or the value of a device key.
    Device,
}

impl From<PreferenceTarget> for PrefTarget {
    fn from(t: PreferenceTarget) -> Self {
        match t {
            PreferenceTarget::Vault => Self::Vault,
            PreferenceTarget::Device => Self::Device,
        }
    }
}

/// See [`sunrise_domain::PrefSource`]: where a resolved value came from, so a
/// settings screen can say "set on this device".
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PreferenceSource {
    /// This device's overlay.
    Overlay,
    /// The synced value.
    Vault,
    /// The key's default.
    Default,
}

impl From<PrefSource> for PreferenceSource {
    fn from(s: PrefSource) -> Self {
        match s {
            PrefSource::Overlay => Self::Overlay,
            PrefSource::Vault => Self::Vault,
            PrefSource::Default => Self::Default,
        }
    }
}

/// One resolved preference. See [`sunrise_domain::ResolvedPref`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PreferenceItem {
    /// The key, e.g. `"week_start"`.
    pub key: String,
    /// Where its value may live.
    pub scope: PreferenceScope,
    /// What it reads as; `None` when it resolves to an absent default.
    pub value: Option<PreferenceValue>,
    /// Where `value` came from.
    pub source: PreferenceSource,
    /// Whether the vault holds a value for it.
    pub vault_set: bool,
}

impl From<ResolvedPref> for PreferenceItem {
    fn from(r: ResolvedPref) -> Self {
        let ResolvedPref {
            key,
            scope,
            value,
            source,
            vault_set,
        } = r;
        Self {
            key: key.to_owned(),
            scope: scope.into(),
            value: value.map(Into::into),
            source: source.into(),
            vault_set,
        }
    }
}

/// The vault's `Preferences` entity as stored: which keys it holds a value
/// for. A client reads values through `CoreQuery::Preferences`, resolved,
/// and never parses a stored one; this mirror exists so the registry's
/// "every synced record has a mirror" holds.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PreferencesItem {
    /// Always the `prf_` zero id.
    pub id: EntityRef,
    /// When the entity was first created.
    pub created_at: jiff::Timestamp,
    /// The newest write.
    pub updated_at: jiff::Timestamp,
    /// Every key the entity holds a value for, known to this build or not.
    pub keys: Vec<String>,
}

impl From<&Preferences> for PreferencesItem {
    fn from(p: &Preferences) -> Self {
        let Preferences {
            id,
            created_at,
            updated_at,
            values,
            unknown: _,
        } = p;
        Self {
            id: *id,
            created_at: *created_at,
            updated_at: *updated_at,
            keys: values.keys().cloned().collect(),
        }
    }
}

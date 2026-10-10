//! Preferences per `docs/02-domain/preferences.md` and ADR-0050.
//!
//! The user's settings are one synced entity per vault, [`Preferences`], plus
//! a device-local overlay the core keeps in its database. This module owns
//! everything about them that is not storage:
//!
//! - the **key table**, [`PREFERENCE_KEYS`]: every key's type, default, scope
//!   and whether it is needed before the vault unlocks;
//! - the **value codec**: [`PrefType::decode`] reads a stored CBOR value back
//!   as a typed [`PrefValue`], refusing what does not fit, and
//!   [`PrefValue::encode`] writes one;
//! - the **resolver**, [`resolve`]: overlay first where the scope permits it,
//!   then the vault value, then the default. A pure function, so every client
//!   reads one answer and none merges values itself.
//!
//! # Lossless
//!
//! [`Preferences::values`] holds every key as raw CBOR, the keys this build
//! does not know included. A value that does not decode under its key's type
//! (a newer build widened the range, or changed the type) is kept as it came
//! and resolves as absent, so it reads as the default here and is never
//! written back as anything else.

use crate::rrule::Weekday;
use crate::unknown::{CborValue, Unknowns};
use crate::validation::ValidationError;
use ciborium::value::Value;
use jiff::civil;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use sunrise_id::{EntityKind, EntityRef};

/// The fixed id body of the one `Preferences` entity in every vault: the zero
/// ULID, so every device writes to the same entity (ADR-0050 §1).
pub const PREFERENCES_BYTES: [u8; 16] = [0u8; 16];

/// The id of the vault's `Preferences` entity, `prf_` and the zero body.
#[must_use]
pub fn preferences_ref() -> EntityRef {
    EntityRef::new(EntityKind::Preferences, PREFERENCES_BYTES)
}

/// The vault's synced preferences: one register per key (ADR-0050 §1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preferences {
    /// Always [`preferences_ref`].
    pub id: EntityRef,
    /// The physical time of the least create the merge applied.
    pub created_at: Timestamp,
    /// The physical time of the newest write.
    pub updated_at: Timestamp,
    /// Every set key's value, as it was written. Keys this build does not
    /// know, and values that do not fit this build's type, are kept here.
    #[serde(default)]
    pub values: BTreeMap<String, CborValue>,
    /// Fields written by a newer `DOC_SCHEMA_V`, preserved verbatim.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

/// Where a key's value may live (ADR-0050 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrefScope {
    /// The synced entity only.
    Vault,
    /// The synced entity, and optionally this device's overlay, which wins.
    VaultOverridable,
    /// This device's overlay only. Never synced.
    Device,
}

impl PrefScope {
    /// Whether a write may name `target` for a key of this scope.
    #[must_use]
    pub const fn permits(self, target: PrefTarget) -> bool {
        matches!(
            (self, target),
            (Self::Vault | Self::VaultOverridable, PrefTarget::Vault)
                | (Self::Device | Self::VaultOverridable, PrefTarget::Device)
        )
    }

    /// The spelling `preferences.md` uses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vault => "vault",
            Self::VaultOverridable => "vault_overridable",
            Self::Device => "device",
        }
    }
}

/// Which store a write goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrefTarget {
    /// The synced entity, as a `Patch`.
    Vault,
    /// This device's overlay. No op.
    Device,
}

/// Where a resolved value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrefSource {
    /// This device's overlay.
    Overlay,
    /// The synced entity.
    Vault,
    /// The key table.
    Default,
}

/// The device classes a default may differ by (`attachments.cache_limit_bytes`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceClass {
    /// A laptop or desktop.
    Desktop,
    /// A phone or a tablet.
    Handheld,
}

impl DeviceClass {
    /// The class of a device by the platform tag its certificate carries
    /// (`"macos"`, `"ios"`, …). An unrecognised tag is a desktop: the larger
    /// cache is the one that never evicts what the user just opened.
    #[must_use]
    pub fn of_platform(platform: &str) -> Self {
        match platform.to_ascii_lowercase().as_str() {
            "ios" | "ipados" | "android" => Self::Handheld,
            _ => Self::Desktop,
        }
    }
}

/// A key's value type, with its range. Each type has one CBOR form, which
/// [`PrefValue::encode`] writes and [`PrefType::decode`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefType {
    /// `bool`.
    Bool,
    /// `uint` in `min..=max`.
    Uint {
        /// Least value.
        min: u64,
        /// Greatest value.
        max: u64,
    },
    /// `uint`, one of the listed values.
    UintOneOf(&'static [u64]),
    /// `int` in `min..=max`.
    Int {
        /// Least value.
        min: i64,
        /// Greatest value.
        max: i64,
    },
    /// `tstr`, one of the listed spellings.
    OneOf(&'static [&'static str]),
    /// `tstr`: an absolute `http`, `https`, `ws` or `wss` URL.
    Url,
    /// `tstr`, non-empty, no surrounding whitespace.
    Text,
    /// `tstr`: an IANA zone name the bundled tzdb resolves.
    TimeZone,
    /// `tstr`: a BCP 47 language tag.
    LanguageTag,
    /// `tstr`: an entity ref of this kind.
    Ref(EntityKind),
    /// `tstr`: a weekday, `"MO"` .. `"SU"`.
    Weekday,
    /// `{ "day": Weekday, "at": civil-time }`.
    Cadence,
    /// `{ "start": civil-time, "end": civil-time }`, `start != end`.
    TimeWindow,
}

/// A typed preference value: what a client sends and is shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrefValue {
    /// [`PrefType::Bool`].
    Bool(bool),
    /// [`PrefType::Uint`] and [`PrefType::UintOneOf`].
    Uint(u64),
    /// [`PrefType::Int`].
    Int(i64),
    /// Every text-valued type.
    Text(String),
    /// [`PrefType::Weekday`].
    Weekday(Weekday),
    /// [`PrefType::Cadence`].
    Cadence {
        /// The day.
        day: Weekday,
        /// The civil time on that day.
        at: civil::Time,
    },
    /// [`PrefType::TimeWindow`].
    TimeWindow {
        /// Civil start.
        start: civil::Time,
        /// Civil end; earlier than `start` wraps midnight.
        end: civil::Time,
    },
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn civil_text(t: civil::Time) -> Value {
    Value::Text(t.strftime("%H:%M:%S").to_string())
}

fn map_get<'a>(entries: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    entries
        .iter()
        .find(|(k, _)| k.as_text() == Some(key))
        .map(|(_, v)| v)
}

fn read_civil(v: &Value) -> Option<civil::Time> {
    v.as_text()?.parse::<civil::Time>().ok()
}

fn read_weekday(v: &Value) -> Option<Weekday> {
    let day = Weekday::from_raw(v.as_text()?);
    (!day.is_unknown()).then_some(day)
}

impl PrefValue {
    /// The CBOR form stored in the entity and the overlay.
    #[must_use]
    pub fn encode(&self) -> CborValue {
        CborValue(match self {
            Self::Bool(b) => Value::Bool(*b),
            Self::Uint(n) => Value::Integer((*n).into()),
            Self::Int(n) => Value::Integer((*n).into()),
            Self::Text(s) => text(s),
            Self::Weekday(d) => text(d.as_str()),
            Self::Cadence { day, at } => Value::Map(vec![
                (text("at"), civil_text(*at)),
                (text("day"), text(day.as_str())),
            ]),
            Self::TimeWindow { start, end } => Value::Map(vec![
                (text("end"), civil_text(*end)),
                (text("start"), civil_text(*start)),
            ]),
        })
    }
}

fn is_url(s: &str) -> bool {
    let rest = ["https://", "http://", "wss://", "ws://"]
        .iter()
        .find_map(|scheme| s.strip_prefix(scheme));
    rest.is_some_and(|r| !r.is_empty() && !r.starts_with('/'))
        && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn is_language_tag(s: &str) -> bool {
    let mut parts = s.split('-');
    let primary = parts.next().unwrap_or_default();
    (2..=8).contains(&primary.len())
        && primary.chars().all(|c| c.is_ascii_alphabetic())
        && parts.all(|p| (1..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

impl PrefType {
    /// Read a stored value as this type. `None` when it does not fit: wrong
    /// shape, out of range, an unknown weekday, a zone the tzdb does not
    /// hold. The caller keeps the stored value and reads the default.
    #[must_use]
    pub fn decode(self, v: &CborValue) -> Option<PrefValue> {
        let v = v.get();
        let value = match self {
            Self::Bool => PrefValue::Bool(v.as_bool()?),
            Self::Uint { .. } | Self::UintOneOf(_) => {
                PrefValue::Uint(u64::try_from(v.as_integer()?).ok()?)
            }
            Self::Int { .. } => PrefValue::Int(i64::try_from(v.as_integer()?).ok()?),
            Self::OneOf(_)
            | Self::Url
            | Self::Text
            | Self::TimeZone
            | Self::LanguageTag
            | Self::Ref(_) => PrefValue::Text(v.as_text()?.to_owned()),
            Self::Weekday => PrefValue::Weekday(read_weekday(v)?),
            Self::Cadence => {
                let m = v.as_map()?;
                PrefValue::Cadence {
                    day: read_weekday(map_get(m, "day")?)?,
                    at: read_civil(map_get(m, "at")?)?,
                }
            }
            Self::TimeWindow => {
                let m = v.as_map()?;
                PrefValue::TimeWindow {
                    start: read_civil(map_get(m, "start")?)?,
                    end: read_civil(map_get(m, "end")?)?,
                }
            }
        };
        self.fits(&value).then_some(value)
    }

    /// Whether `value` is of this type and inside its range.
    #[must_use]
    pub fn fits(self, value: &PrefValue) -> bool {
        match (self, value) {
            (Self::Bool, PrefValue::Bool(_)) => true,
            (Self::Uint { min, max }, PrefValue::Uint(n)) => (min..=max).contains(n),
            (Self::UintOneOf(allowed), PrefValue::Uint(n)) => allowed.contains(n),
            (Self::Int { min, max }, PrefValue::Int(n)) => (min..=max).contains(n),
            (Self::OneOf(allowed), PrefValue::Text(s)) => allowed.contains(&s.as_str()),
            (Self::Url, PrefValue::Text(s)) => is_url(s),
            (Self::Text, PrefValue::Text(s)) => !s.is_empty() && s.trim() == s,
            (Self::TimeZone, PrefValue::Text(s)) => jiff::tz::TimeZone::get(s).is_ok(),
            (Self::LanguageTag, PrefValue::Text(s)) => is_language_tag(s),
            (Self::Ref(kind), PrefValue::Text(s)) => EntityRef::parse(s, kind).is_ok(),
            (Self::Weekday, PrefValue::Weekday(d))
            | (Self::Cadence, PrefValue::Cadence { day: d, .. }) => !d.is_unknown(),
            (Self::TimeWindow, PrefValue::TimeWindow { start, end }) => start != end,
            _ => false,
        }
    }
}

/// A key's default, as a constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefDefault {
    /// Unset: the key has no value until one is written.
    Absent,
    /// A `bool`.
    Bool(bool),
    /// A `uint`.
    Uint(u64),
    /// An `int`.
    Int(i64),
    /// A text value.
    Text(&'static str),
    /// A weekday, by its spelling.
    Weekday(&'static str),
    /// A cadence: a weekday spelling and a civil time.
    Cadence(&'static str, &'static str),
    /// A `uint` that differs by device class.
    UintByClass {
        /// On a laptop or desktop.
        desktop: u64,
        /// On a phone or tablet.
        handheld: u64,
    },
}

impl PrefDefault {
    /// The typed default on a device of `class`.
    #[must_use]
    pub fn value(self, class: DeviceClass) -> Option<PrefValue> {
        Some(match self {
            Self::Absent => return None,
            Self::Bool(b) => PrefValue::Bool(b),
            Self::Uint(n) => PrefValue::Uint(n),
            Self::Int(n) => PrefValue::Int(n),
            Self::Text(s) => PrefValue::Text(s.to_owned()),
            Self::Weekday(d) => PrefValue::Weekday(Weekday::from_raw(d)),
            Self::Cadence(d, at) => PrefValue::Cadence {
                day: Weekday::from_raw(d),
                at: at.parse().ok()?,
            },
            Self::UintByClass { desktop, handheld } => PrefValue::Uint(match class {
                DeviceClass::Desktop => desktop,
                DeviceClass::Handheld => handheld,
            }),
        })
    }
}

/// One key of the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefSpec {
    /// The key, e.g. `"week_start"`. Never renamed.
    pub key: &'static str,
    /// Its value type.
    pub ty: PrefType,
    /// What it reads as with no value.
    pub default: PrefDefault,
    /// Where its value may live.
    pub scope: PrefScope,
    /// Needed before the vault unlocks, so kept in the plaintext bootstrap
    /// file rather than the encrypted overlay. Always `device`-scoped, and
    /// never user content.
    pub bootstrap: bool,
}

mod keys;
pub use keys::PREFERENCE_KEYS;

/// The key table's entry for `key`, or `None` for a key this build does
/// not know.
#[must_use]
pub fn pref_spec(key: &str) -> Option<&'static PrefSpec> {
    PREFERENCE_KEYS.iter().find(|s| s.key == key)
}

/// Whether `key` is a well-formed preference key:
/// `[a-z][a-z0-9_]*(\.[A-Za-z0-9_-]+)*`, the CDDL of `preferences.md`.
#[must_use]
pub fn is_pref_key(key: &str) -> bool {
    let mut segments = key.split('.');
    let first = segments.next().unwrap_or_default();
    let mut chars = first.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && segments.all(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

/// Check a local write before anything is stored: the key is known and not
/// a bootstrap key, the scope permits the target, and the value fits.
///
/// # Errors
/// [`ValidationError::PreferenceScope`] for a target the key's scope does not
/// permit; [`ValidationError::Field`] for an unknown key, a bootstrap key
/// (which the bootstrap store owns), or a value that does not fit.
pub fn check_write(
    key: &str,
    value: Option<&PrefValue>,
    target: PrefTarget,
) -> Result<&'static PrefSpec, ValidationError> {
    let spec = pref_spec(key).ok_or(ValidationError::Field {
        field: "preference.key",
        constraint: "unknown",
    })?;
    if spec.bootstrap {
        return Err(ValidationError::Field {
            field: "preference.key",
            constraint: "bootstrap",
        });
    }
    if !spec.scope.permits(target) {
        return Err(ValidationError::PreferenceScope);
    }
    if let Some(v) = value {
        if !spec.ty.fits(v) {
            return Err(ValidationError::Field {
                field: "preference.value",
                constraint: "type",
            });
        }
    }
    Ok(spec)
}

/// One key, resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedPref {
    /// The key.
    pub key: &'static str,
    /// Its scope.
    pub scope: PrefScope,
    /// The value it reads as. `None` when it resolves to an absent default.
    pub value: Option<PrefValue>,
    /// Where `value` came from.
    pub source: PrefSource,
    /// Whether the vault holds a value for it, valid or not. A settings
    /// screen offers "use the account value" only when one exists.
    pub vault_set: bool,
}

/// Resolve one key from this device's overlay value and the vault's value
/// (ADR-0050 §2):
///
/// ```text
/// scope ∈ {device, vault_overridable} and overlay has a valid value → overlay
/// scope ∈ {vault, vault_overridable}  and vault has a valid value   → vault
/// otherwise                                                         → default
/// ```
///
/// A value of the wrong type for its key's scope is never read: a vault value
/// of a `device` key, or an overlay value of a `vault` key, is ignored.
#[must_use]
pub fn resolve(
    spec: &'static PrefSpec,
    overlay: Option<&CborValue>,
    vault: Option<&CborValue>,
    class: DeviceClass,
) -> ResolvedPref {
    let read = |v: Option<&CborValue>| v.and_then(|v| spec.ty.decode(v));
    let (value, source) = match spec.scope {
        PrefScope::Device => (read(overlay), PrefSource::Overlay),
        PrefScope::Vault => (read(vault), PrefSource::Vault),
        PrefScope::VaultOverridable => match read(overlay) {
            Some(v) => (Some(v), PrefSource::Overlay),
            None => (read(vault), PrefSource::Vault),
        },
    };
    let (value, source) = match value {
        Some(v) => (Some(v), source),
        None => (spec.default.value(class), PrefSource::Default),
    };
    ResolvedPref {
        key: spec.key,
        scope: spec.scope,
        value,
        source,
        vault_set: spec.scope != PrefScope::Device && vault.is_some(),
    }
}

/// Every non-bootstrap key, resolved against `overlay` and `vault`, in table
/// order. Keys either map holds that the table does not are kept by their
/// stores and not returned.
#[must_use]
pub fn resolve_all(
    overlay: &BTreeMap<String, CborValue>,
    vault: &BTreeMap<String, CborValue>,
    class: DeviceClass,
) -> Vec<ResolvedPref> {
    PREFERENCE_KEYS
        .iter()
        .filter(|s| !s.bootstrap)
        .map(|s| resolve(s, overlay.get(s.key), vault.get(s.key), class))
        .collect()
}

#[cfg(test)]
mod tests;

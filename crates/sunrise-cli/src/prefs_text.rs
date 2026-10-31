//! A preference value as typed on a command line, and as printed back.
//!
//! `sunrise prefs set <key> <value>` takes one argument, so each of the key
//! table's value types ([`sunrise_domain::PrefType`]) has one spelling here,
//! and [`render`] prints that same spelling: what `prefs get` shows is what
//! `prefs set` accepts. The range and the shape are checked against the key's
//! own type before anything is sent, and the core checks them again.
//!
//! | Type | Spelling |
//! |---|---|
//! | bool | `true` / `false` (also `on`/`off`, `yes`/`no`) |
//! | uint, int | a decimal integer |
//! | weekday | `MO` .. `SU` |
//! | cadence | `MO 09:00` |
//! | time window | `22:00-07:00` |
//! | day window (wake-sleep) | `07:00-01:00` |
//! | day override | a day window, or `none` |
//! | every text type | the text itself |

use sunrise_domain::{PrefSpec, PrefType, PrefValue, Weekday};

use crate::i18n::strings;

/// Why a typed value was not accepted for a key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PrefTextError {
    /// The key table does not know this key.
    #[error("{}", strings::prefs::unknown_key(.0))]
    UnknownKey(String),
    /// The text is not a value of the key's type, or is outside its range.
    #[error("{}", strings::prefs::bad_value(.value, .key, .expected))]
    BadValue {
        /// The key.
        key: String,
        /// What was typed.
        value: String,
        /// The spelling the key takes, worded for the reader.
        expected: String,
    },
}

/// The spelling a value of `ty` is typed in, for an error message.
#[must_use]
pub fn expected(ty: PrefType) -> String {
    match ty {
        PrefType::Bool => strings::prefs::expect_bool(),
        PrefType::Uint { .. } | PrefType::UintOneOf(_) | PrefType::Int { .. } => {
            strings::prefs::expect_number()
        }
        PrefType::OneOf(allowed) => strings::prefs::expect_one_of(&allowed.join(", ")),
        PrefType::Url => strings::prefs::expect_url(),
        PrefType::Text => strings::prefs::expect_text(),
        PrefType::TimeZone => strings::prefs::expect_zone(),
        PrefType::LanguageTag => strings::prefs::expect_language(),
        PrefType::Ref(_) => strings::prefs::expect_id(),
        PrefType::Weekday => strings::prefs::expect_weekday(),
        PrefType::Cadence => strings::prefs::expect_cadence(),
        PrefType::TimeWindow => strings::prefs::expect_window(),
        PrefType::DayWindow => strings::prefs::expect_day_window(),
        PrefType::DayOverride => strings::prefs::expect_day_override(),
    }
}

/// The key table's entry for `key`.
///
/// # Errors
/// [`PrefTextError::UnknownKey`].
pub fn spec(key: &str) -> Result<&'static PrefSpec, PrefTextError> {
    sunrise_domain::pref_spec(key).ok_or_else(|| PrefTextError::UnknownKey(key.to_owned()))
}

/// `text` as a value of `spec`'s type, checked against its range.
///
/// # Errors
/// [`PrefTextError::BadValue`].
pub fn parse(spec: &PrefSpec, text: &str) -> Result<PrefValue, PrefTextError> {
    parse_for(spec.key, spec, text)
}

/// [`parse`], naming `key` in the error: the entry typed, where `spec` is
/// the map it belongs to (`day_schedule.weekday.MO` of `day_schedule.weekday`).
///
/// # Errors
/// [`PrefTextError::BadValue`].
pub fn parse_for(key: &str, spec: &PrefSpec, text: &str) -> Result<PrefValue, PrefTextError> {
    let bad = || PrefTextError::BadValue {
        key: key.to_owned(),
        value: text.to_owned(),
        expected: expected(spec.ty),
    };
    let t = text.trim();
    let value = match spec.ty {
        PrefType::Bool => PrefValue::Bool(match t.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" => true,
            "false" | "off" | "no" => false,
            _ => return Err(bad()),
        }),
        PrefType::Uint { .. } | PrefType::UintOneOf(_) => {
            PrefValue::Uint(t.parse().map_err(|_| bad())?)
        }
        PrefType::Int { .. } => PrefValue::Int(t.parse().map_err(|_| bad())?),
        PrefType::Weekday => PrefValue::Weekday(weekday(t).ok_or_else(bad)?),
        PrefType::Cadence => {
            let (day, at) = t.split_once(char::is_whitespace).ok_or_else(bad)?;
            PrefValue::Cadence {
                day: weekday(day).ok_or_else(bad)?,
                at: civil_time(at).ok_or_else(bad)?,
            }
        }
        PrefType::TimeWindow => {
            let (start, end) = t.split_once('-').ok_or_else(bad)?;
            PrefValue::TimeWindow {
                start: civil_time(start).ok_or_else(bad)?,
                end: civil_time(end).ok_or_else(bad)?,
            }
        }
        PrefType::DayOverride if t.eq_ignore_ascii_case("none") => PrefValue::NoSchedule,
        PrefType::DayWindow | PrefType::DayOverride => {
            let (wake, sleep) = t.split_once('-').ok_or_else(bad)?;
            PrefValue::DayWindow {
                wake: civil_time(wake).ok_or_else(bad)?,
                sleep: civil_time(sleep).ok_or_else(bad)?,
            }
        }
        PrefType::OneOf(_)
        | PrefType::Url
        | PrefType::Text
        | PrefType::TimeZone
        | PrefType::LanguageTag
        | PrefType::Ref(_) => PrefValue::Text(t.to_owned()),
    };
    if spec.ty.fits(&value) {
        Ok(value)
    } else {
        Err(bad())
    }
}

/// `value` in the spelling [`parse`] reads.
#[must_use]
pub fn render(value: &PrefValue) -> String {
    match value {
        PrefValue::Bool(b) => b.to_string(),
        PrefValue::Uint(n) => n.to_string(),
        PrefValue::Int(n) => n.to_string(),
        PrefValue::Text(s) => s.clone(),
        PrefValue::Weekday(d) => d.as_str().to_owned(),
        PrefValue::Cadence { day, at } => format!("{} {}", day.as_str(), hm(*at)),
        PrefValue::TimeWindow { start, end } => format!("{}-{}", hm(*start), hm(*end)),
        PrefValue::DayWindow { wake, sleep } => format!("{}-{}", hm(*wake), hm(*sleep)),
        PrefValue::NoSchedule => "none".to_owned(),
    }
}

/// `t` as `HH:MM`, the spelling every civil time here is typed in.
#[must_use]
pub fn hm(t: jiff::civil::Time) -> String {
    t.strftime("%H:%M").to_string()
}

fn weekday(s: &str) -> Option<Weekday> {
    Weekday::parse(&s.trim().to_ascii_uppercase())
}

fn civil_time(s: &str) -> Option<jiff::civil::Time> {
    s.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_domain::{DeviceClass, PREFERENCE_KEYS};

    fn key(k: &str) -> &'static PrefSpec {
        spec(k).unwrap()
    }

    #[test]
    fn each_type_reads_its_spelling_and_prints_it_back() {
        for (k, text) in [
            ("week_start", "MO"),
            ("time_format", "h24"),
            ("stale_after_days", "21"),
            ("home_timezone", "Asia/Kolkata"),
            ("day_schedule.weekday.MO", "07:00-01:00"),
            ("day_schedule.month_day.-1", "05:00-22:00"),
            ("day_schedule.date.2026-12-25", "none"),
            ("day_schedule.date.2026-12-24", "09:00-23:30"),
        ] {
            let v = parse(key(k), text).unwrap_or_else(|e| panic!("{k}: {e}"));
            assert_eq!(render(&v), text, "{k}");
        }
    }

    #[test]
    fn a_value_outside_the_keys_range_is_refused_before_anything_is_sent() {
        assert!(matches!(
            parse(key("stale_after_days"), "9999"),
            Err(PrefTextError::BadValue { .. })
        ));
        assert!(parse(key("time_format"), "h13").is_err());
        assert!(parse(key("week_start"), "XX").is_err());
        assert!(parse(key("home_timezone"), "Mars/Olympus").is_err());
        assert!(parse(key("day_schedule.weekday.MO"), "07:00-07:00").is_err());
        assert!(parse(key("day_schedule.weekday.MO"), "none").is_err());
    }

    #[test]
    fn a_key_the_table_does_not_know_is_named() {
        assert_eq!(
            spec("no_such_key").unwrap_err(),
            PrefTextError::UnknownKey("no_such_key".into())
        );
    }

    #[test]
    fn the_structured_spellings_round_trip() {
        let at = jiff::civil::time(9, 0, 0, 0);
        assert_eq!(
            render(&PrefValue::Cadence {
                day: Weekday::Mo,
                at
            }),
            "MO 09:00"
        );
        assert_eq!(
            render(&PrefValue::TimeWindow {
                start: jiff::civil::time(22, 0, 0, 0),
                end: jiff::civil::time(7, 0, 0, 0)
            }),
            "22:00-07:00"
        );

        // Read back, through the keys of those types.
        let of_type = |wanted: fn(PrefType) -> bool| {
            PREFERENCE_KEYS
                .iter()
                .find(|s| wanted(s.ty))
                .expect("the table has a key of this type")
        };
        assert_eq!(
            parse(of_type(|t| matches!(t, PrefType::Cadence)), "MO 09:00").unwrap(),
            PrefValue::Cadence {
                day: Weekday::Mo,
                at
            }
        );
        assert_eq!(
            parse(
                of_type(|t| matches!(t, PrefType::TimeWindow)),
                "22:00-07:00"
            )
            .unwrap(),
            PrefValue::TimeWindow {
                start: jiff::civil::time(22, 0, 0, 0),
                end: jiff::civil::time(7, 0, 0, 0)
            }
        );
    }

    /// Values of `ty` covering each shape it takes and both ends of its range.
    fn samples(ty: PrefType) -> Vec<PrefValue> {
        let time = |h, m| jiff::civil::time(h, m, 0, 0);
        match ty {
            PrefType::Bool => vec![PrefValue::Bool(true), PrefValue::Bool(false)],
            PrefType::Uint { min, max } => vec![PrefValue::Uint(min), PrefValue::Uint(max)],
            PrefType::UintOneOf(allowed) => allowed.iter().copied().map(PrefValue::Uint).collect(),
            PrefType::Int { min, max } => vec![PrefValue::Int(min), PrefValue::Int(max)],
            PrefType::OneOf(allowed) => allowed
                .iter()
                .map(|a| PrefValue::Text((*a).to_owned()))
                .collect(),
            PrefType::Url => vec![PrefValue::Text("https://relay.example/sync".into())],
            PrefType::Text => vec![PrefValue::Text("two words".into())],
            PrefType::TimeZone => vec![PrefValue::Text("Asia/Kolkata".into())],
            PrefType::LanguageTag => vec![PrefValue::Text("fr-CA".into())],
            PrefType::Ref(kind) => vec![PrefValue::Text(
                sunrise_id::EntityRef::new(kind, [7; 16]).to_str(),
            )],
            PrefType::Weekday => sunrise_domain::day_schedule::WEEKDAYS
                .iter()
                .cloned()
                .map(PrefValue::Weekday)
                .collect(),
            PrefType::Cadence => sunrise_domain::day_schedule::WEEKDAYS
                .iter()
                .cloned()
                .map(|day| PrefValue::Cadence {
                    day,
                    at: time(9, 5),
                })
                .collect(),
            PrefType::TimeWindow => vec![PrefValue::TimeWindow {
                start: time(22, 0),
                end: time(7, 30),
            }],
            PrefType::DayWindow => vec![PrefValue::DayWindow {
                wake: time(7, 0),
                sleep: time(1, 0),
            }],
            PrefType::DayOverride => vec![
                PrefValue::DayWindow {
                    wake: time(7, 0),
                    sleep: time(23, 30),
                },
                PrefValue::NoSchedule,
            ],
        }
    }

    /// The module's promise, for the whole key table rather than a sample of
    /// it: what `prefs get` shows is what `prefs set` accepts, and reads back
    /// as the same value. That holds for each key's default too, since a
    /// default is what `prefs get` shows on a fresh vault.
    #[test]
    fn every_key_reads_back_what_it_prints() {
        assert!(!PREFERENCE_KEYS.is_empty());
        for s in PREFERENCE_KEYS {
            let mut values = samples(s.ty);
            assert!(!values.is_empty(), "{} has nothing to try", s.key);
            for class in [DeviceClass::Desktop, DeviceClass::Handheld] {
                values.extend(s.default.value(class));
            }
            for value in values {
                let text = render(&value);
                assert_eq!(
                    parse(s, &text),
                    Ok(value),
                    "{} does not read back {text:?}",
                    s.key
                );
            }
        }
    }
}

//! A picked moment turned into a time value of the kind the user meant.
//!
//! A client's pickers and grids speak epoch milliseconds: a date picker hands
//! back an instant, and a drag on a calendar column is a distance from the
//! top of a day. What is stored is a [`SunriseTime`], and its kind is the
//! user's meaning (`docs/10-cross-cutting/time.md` §1): 09:00 here, 09:00
//! wherever I am, a fixed moment, or a date. Every function here goes from
//! one to the other **in the zone the caller names**, and keeps a stored kind
//! unless the caller asks for a different one
//! (`docs/11-adr/0066-times-survive-travel.md` §4).
//!
//! Nothing falls back silently (ADR-0066 §3). A zone name the tzdb does not
//! know is [`CivilError::UnknownZone`], not UTC, and a moment with no civil
//! reading is an error, not the epoch.

use jiff::tz::TimeZone;
use jiff::Timestamp;
use sunrise_domain::{SunriseTime, Unresolvable};
use thiserror::Error;

/// Why a picked moment could not become a time value.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CivilError {
    /// The caller named a zone the tzdb does not know. Reading it as UTC
    /// would write a confident wrong answer (ADR-0066 §3).
    #[error("unknown time zone: {name}")]
    UnknownZone {
        /// The name as given.
        name: String,
    },
    /// The milliseconds are outside the range a timestamp can hold.
    #[error("not a representable instant: {ms} ms")]
    OutOfRange {
        /// The milliseconds as given.
        ms: i64,
    },
    /// The domain could not resolve or move the value: a kind this build
    /// does not know, or a result off the end of the calendar.
    #[error(transparent)]
    Unresolvable(#[from] Unresolvable),
}

/// The four kinds of time a picker can ask for. [`SunriseTime::Unknown`] is
/// not one: a client never chooses a kind this build does not know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeKind {
    /// A fixed point on the timeline. Moves for nobody.
    Instant,
    /// A wall-clock time in the reader's zone, named. Travels with the place.
    Zoned,
    /// A wall-clock time wherever the reader is. Travels with the reader.
    Floating,
    /// A date with no time of day.
    AllDay,
}

impl TimeKind {
    /// Which kind `value` is; `None` for a kind this build does not know.
    #[must_use]
    pub const fn of(value: &SunriseTime) -> Option<Self> {
        match value {
            SunriseTime::Instant { .. } => Some(Self::Instant),
            SunriseTime::Zoned { .. } => Some(Self::Zoned),
            SunriseTime::Floating { .. } => Some(Self::Floating),
            SunriseTime::AllDay { .. } => Some(Self::AllDay),
            SunriseTime::Unknown { .. } => None,
        }
    }
}

/// A moment a picker or a grid produced, and the kind the user asked for.
///
/// `kind` is `None` when the form offers no kind picker: the stored value
/// then keeps the kind it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickedTime {
    /// The moment, epoch milliseconds.
    pub ms: i64,
    /// The kind asked for, or `None` to keep the stored one.
    pub kind: Option<TimeKind>,
}

/// The reader's zone, by IANA name. No fallback: an unknown name is an error.
///
/// # Errors
/// [`CivilError::UnknownZone`] when the tzdb does not know `name`.
pub fn reader_zone(name: &str) -> Result<TimeZone, CivilError> {
    TimeZone::get(name).map_err(|_| CivilError::UnknownZone {
        name: name.to_owned(),
    })
}

/// `ms` as a timestamp.
///
/// # Errors
/// [`CivilError::OutOfRange`] past the ends of the timestamp range.
pub fn instant(ms: i64) -> Result<Timestamp, CivilError> {
    Timestamp::from_millisecond(ms).map_err(|_| CivilError::OutOfRange { ms })
}

/// Where a picker seeded from `value` starts, epoch milliseconds, in
/// `reader`.
///
/// The value's own instant when it has one (an all-day date at the start of
/// its day). A value with none — a kind this build does not know, carrying no
/// anchor — seeds at the instant a sort places it
/// ([`SunriseTime::instant_or_last`]), never the epoch, and the editors
/// compare against this same seed, so a picker left alone keeps the value
/// byte for byte.
#[must_use]
pub fn seed_ms(value: &SunriseTime, reader: &TimeZone) -> i64 {
    value.instant_or_last(reader).as_millisecond()
}

/// The moment `ms`, expressed as `kind`, read in `reader`.
///
/// | Kind | Value |
/// |---|---|
/// | `Instant` | the moment itself |
/// | `Zoned` | its wall-clock time in `reader`, and `reader`'s name |
/// | `Floating` | its wall-clock time in `reader`, and no zone |
/// | `AllDay` | its date in `reader` |
///
/// The wall clock keeps the moment's milliseconds, so a value written here
/// resolves back to `ms` in `reader`.
///
/// # Errors
/// [`CivilError::OutOfRange`] for milliseconds no timestamp holds.
pub fn value_at(ms: i64, kind: TimeKind, reader: &TimeZone) -> Result<SunriseTime, CivilError> {
    let at = instant(ms)?;
    let civil = at.to_zoned(reader.clone()).datetime();
    Ok(match kind {
        TimeKind::Instant => SunriseTime::instant(at),
        TimeKind::Zoned => SunriseTime::zoned(civil, zone_name(reader)),
        TimeKind::Floating => SunriseTime::floating(civil),
        TimeKind::AllDay => SunriseTime::all_day(civil.date()),
    })
}

/// `existing` moved to the moment `ms`, keeping its kind.
///
/// A zoned value keeps **its own** zone, not the reader's: re-pinning a
/// Berlin 09:00 to the zone of whichever device moved it is the flattening
/// ADR-0066 §4 forbids. A zone the tzdb does not know keeps its name and is
/// read in `reader`, which is where the core resolves it too (ADR-0066 §2).
/// `None` for an [`SunriseTime::Unknown`] value: this build cannot write its
/// kind at another moment, so the caller decides what a move of it means.
///
/// # Errors
/// [`CivilError::OutOfRange`] for milliseconds no timestamp holds.
pub fn same_kind_at(
    existing: &SunriseTime,
    ms: i64,
    reader: &TimeZone,
) -> Result<Option<SunriseTime>, CivilError> {
    let at = instant(ms)?;
    Ok(match existing {
        SunriseTime::Instant { .. } => Some(SunriseTime::instant(at)),
        SunriseTime::Zoned { tz, .. } => {
            let zone = TimeZone::get(tz).unwrap_or_else(|_| reader.clone());
            Some(SunriseTime::zoned(at.to_zoned(zone).datetime(), tz.clone()))
        }
        SunriseTime::Floating { .. } => Some(SunriseTime::floating(
            at.to_zoned(reader.clone()).datetime(),
        )),
        SunriseTime::AllDay { .. } => {
            Some(SunriseTime::all_day(at.to_zoned(reader.clone()).date()))
        }
        SunriseTime::Unknown { .. } => None,
    })
}

/// The edit a form makes to one optional time field: `None` leaves it alone,
/// `Some(None)` clears it, `Some(Some(v))` sets it — the domain patches'
/// shape.
///
/// * Absent before and after: no edit.
/// * Present before, absent after: clear.
/// * Absent before: set, as `picked.kind` or else `default`.
/// * Present before and after: no edit unless the moment moved from its seed
///   ([`seed_ms`]) or the kind changed. A move keeps the stored kind (and
///   zone) through [`same_kind_at`]; a changed kind is written fresh by
///   [`value_at`]. A move of an unknown kind, which cannot keep its kind, is
///   written as `default`. A result equal to the stored value is no edit, so
///   moving an all-day date's time of day changes nothing.
///
/// # Errors
/// What [`value_at`] and [`same_kind_at`] raise.
#[allow(clippy::option_option)]
pub fn field_edit(
    stored: Option<&SunriseTime>,
    picked: Option<PickedTime>,
    default: TimeKind,
    reader: &TimeZone,
) -> Result<Option<Option<SunriseTime>>, CivilError> {
    let (stored, picked) = match (stored, picked) {
        (None, None) => return Ok(None),
        (Some(_), None) => return Ok(Some(None)),
        (None, Some(p)) => {
            return value_at(p.ms, p.kind.unwrap_or(default), reader).map(|v| Some(Some(v)))
        }
        (Some(s), Some(p)) => (s, p),
    };
    let stored_kind = TimeKind::of(stored);
    let kind_changed = picked.kind.is_some_and(|k| Some(k) != stored_kind);
    if !kind_changed && picked.ms == seed_ms(stored, reader) {
        return Ok(None);
    }
    let next = match (kind_changed, same_kind_at(stored, picked.ms, reader)?) {
        (false, Some(kept)) => kept,
        _ => value_at(picked.ms, picked.kind.unwrap_or(default), reader)?,
    };
    Ok((next != *stored).then_some(Some(next)))
}

/// The IANA name of `zone`, for a zoned value. Every zone reached here came
/// from [`reader_zone`], so it has one; `UTC` stands in for the fixed-offset
/// zones that do not, which no caller constructs.
fn zone_name(zone: &TimeZone) -> String {
    zone.iana_name().unwrap_or("UTC").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn toronto() -> TimeZone {
        reader_zone("America/Toronto").unwrap()
    }

    /// 2026-03-07T14:00:00Z, 09:00 in Toronto the day before spring-forward.
    const SAT_NINE_TORONTO: i64 = 1_772_892_000_000;

    #[test]
    fn an_unknown_zone_is_an_error_not_utc() {
        assert_eq!(
            reader_zone("Not/AZone"),
            Err(CivilError::UnknownZone {
                name: "Not/AZone".into()
            })
        );
        assert!(matches!(
            value_at(i64::MAX, TimeKind::Instant, &toronto()),
            Err(CivilError::OutOfRange { .. })
        ));
    }

    #[test]
    fn each_kind_is_written_as_asked_and_reads_back_to_the_moment() {
        let tz = toronto();
        let nine = date(2026, 3, 7).at(9, 0, 0, 0);
        let cases = [
            (
                TimeKind::Instant,
                SunriseTime::instant(instant(SAT_NINE_TORONTO).unwrap()),
            ),
            (TimeKind::Zoned, SunriseTime::zoned(nine, "America/Toronto")),
            (TimeKind::Floating, SunriseTime::floating(nine)),
            (TimeKind::AllDay, SunriseTime::all_day(date(2026, 3, 7))),
        ];
        for (kind, want) in cases {
            let got = value_at(SAT_NINE_TORONTO, kind, &tz).unwrap();
            assert_eq!(got, want, "{kind:?}");
            assert_eq!(TimeKind::of(&got), Some(kind));
        }
    }

    /// A zoned value moved on a device elsewhere keeps its own zone and
    /// reads its new wall clock there.
    #[test]
    fn a_zoned_value_moves_in_its_own_zone() {
        let berlin = SunriseTime::zoned(date(2026, 3, 7).at(9, 0, 0, 0), "Europe/Berlin");
        // 2026-03-07T09:00:00Z is 10:00 in Berlin.
        let moved = same_kind_at(&berlin, 1_772_874_000_000, &toronto())
            .unwrap()
            .unwrap();
        assert_eq!(
            moved,
            SunriseTime::zoned(date(2026, 3, 7).at(10, 0, 0, 0), "Europe/Berlin")
        );
    }

    #[test]
    fn an_untouched_field_is_no_edit_whatever_its_kind() {
        let tz = toronto();
        let unknown = SunriseTime::Unknown {
            kind: "lunar".into(),
            raw: sunrise_domain::Unknowns::default(),
        };
        let values = [
            SunriseTime::all_day(date(2026, 3, 7)),
            SunriseTime::floating(date(2026, 3, 8).at(2, 30, 0, 0)),
            SunriseTime::zoned(date(2026, 3, 7).at(9, 0, 0, 0), "Asia/Kolkata"),
            SunriseTime::zoned(date(2026, 3, 7).at(9, 0, 0, 0), "Not/AZone"),
            unknown,
        ];
        for v in values {
            let picked = PickedTime {
                ms: seed_ms(&v, &tz),
                kind: None,
            };
            assert!(
                field_edit(Some(&v), Some(picked), TimeKind::Floating, &tz)
                    .unwrap()
                    .is_none(),
                "{v:?}"
            );
        }
    }

    #[test]
    fn a_moved_field_keeps_its_kind_and_a_new_one_takes_the_default() {
        let tz = toronto();
        let day = SunriseTime::all_day(date(2026, 3, 7));
        let next_day = seed_ms(&day, &tz) + 86_400_000 + 3_600_000;
        let moved = field_edit(
            Some(&day),
            Some(PickedTime {
                ms: next_day,
                kind: None,
            }),
            TimeKind::Floating,
            &tz,
        )
        .unwrap();
        assert_eq!(moved, Some(Some(SunriseTime::all_day(date(2026, 3, 8)))));

        let same_day = field_edit(
            Some(&day),
            Some(PickedTime {
                ms: seed_ms(&day, &tz) + 9 * 3_600_000,
                kind: None,
            }),
            TimeKind::Floating,
            &tz,
        )
        .unwrap();
        assert_eq!(
            same_day, None,
            "a date given a time of day is still that date"
        );

        let fresh = field_edit(
            None,
            Some(PickedTime {
                ms: SAT_NINE_TORONTO,
                kind: None,
            }),
            TimeKind::Floating,
            &tz,
        )
        .unwrap();
        assert_eq!(
            fresh,
            Some(Some(SunriseTime::floating(date(2026, 3, 7).at(9, 0, 0, 0))))
        );
        assert_eq!(
            field_edit(Some(&day), None, TimeKind::Floating, &tz).unwrap(),
            Some(None)
        );
        assert_eq!(
            field_edit(None, None, TimeKind::Floating, &tz).unwrap(),
            None
        );
    }

    #[test]
    fn a_changed_kind_is_written_even_at_the_same_moment() {
        let tz = toronto();
        let floating = SunriseTime::floating(date(2026, 3, 7).at(9, 0, 0, 0));
        let pinned = field_edit(
            Some(&floating),
            Some(PickedTime {
                ms: SAT_NINE_TORONTO,
                kind: Some(TimeKind::Zoned),
            }),
            TimeKind::Floating,
            &tz,
        )
        .unwrap();
        assert_eq!(
            pinned,
            Some(Some(SunriseTime::zoned(
                date(2026, 3, 7).at(9, 0, 0, 0),
                "America/Toronto"
            )))
        );
    }

    /// An unknown kind that the user did move has no kind to keep; it is an
    /// explicit set, written as the default.
    #[test]
    fn a_moved_unknown_kind_takes_the_default() {
        let tz = toronto();
        let unknown = SunriseTime::Unknown {
            kind: "lunar".into(),
            raw: sunrise_domain::Unknowns::default(),
        };
        let set = field_edit(
            Some(&unknown),
            Some(PickedTime {
                ms: SAT_NINE_TORONTO,
                kind: None,
            }),
            TimeKind::Floating,
            &tz,
        )
        .unwrap();
        assert_eq!(
            set,
            Some(Some(SunriseTime::floating(date(2026, 3, 7).at(9, 0, 0, 0))))
        );
    }
}

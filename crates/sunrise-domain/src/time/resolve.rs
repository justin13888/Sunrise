//! What a caller does when a time has no instant to give.
//!
//! `docs/11-adr/0066-times-survive-travel.md` §3: a conversion that cannot
//! produce an instant returns an error, never the epoch, and every caller
//! decides what that means for its reader. The parent module's
//! [`SunriseTime::to_instant`] returns the error; this module holds the error
//! and the two policies more than one caller shares.

use super::{SunriseTime, UNANCHORED_MS};
use jiff::tz::TimeZone;
use jiff::Timestamp;

/// Why a [`SunriseTime`] has no instant (`docs/11-adr/0066-times-survive-travel.md`
/// §3).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unresolvable {
    /// A kind this build does not know. It has an instant only when it
    /// carries an `at` anchor, and it is never moved
    /// ([`super::shift_civil_days`]): rewriting a value this build cannot read
    /// would lose what it means.
    #[error("a time of unknown kind {kind:?} cannot be placed or moved here")]
    UnknownKind {
        /// The kind, as it arrived.
        kind: String,
    },
    /// A civil value at the edge of the calendar with no instant in the zone
    /// it resolves in: `9999-12-31T23:00` east of UTC, say.
    #[error("{value} has no instant in the timestamp range")]
    OutOfRange {
        /// The value, rendered with its kind.
        value: String,
    },
}

impl SunriseTime {
    /// Whether this is a `Zoned` value naming a zone this build cannot
    /// resolve. It resolves in the reader's zone and keeps its stored zone
    /// name unchanged; a view flags it (`docs/10-cross-cutting/time.md` §3).
    #[must_use]
    pub fn zone_unknown(&self) -> bool {
        matches!(self, Self::Zoned { tz, .. } if TimeZone::get(tz).is_err())
    }

    /// The instant a sort places this value at for a reader in `tz`: its
    /// resolved instant, or [`UNANCHORED_MS`] when it has none, so a value
    /// this build cannot place sorts after every value it can and never reads
    /// as overdue or past (`docs/11-adr/0066-times-survive-travel.md` §3, "a
    /// sort places the value last").
    ///
    /// For an ordering or a single number a caller must have. A decision that
    /// can say "no answer" uses [`SunriseTime::resolve_in`] instead.
    #[must_use]
    pub fn instant_or_last(&self, tz: &TimeZone) -> Timestamp {
        self.to_instant(tz).unwrap_or_else(|_| last())
    }

    /// The storage key for a civil value with no instant even in UTC: the
    /// nearer end of the timestamp range, judged by the value's own year. A
    /// stable key that keeps the value at its end of a range scan, where the
    /// epoch would drop it into 1970.
    pub(super) fn edge_index_ms(&self) -> i64 {
        let year = match self {
            Self::Zoned { civil, .. } | Self::Floating { civil } => civil.year(),
            Self::AllDay { date } => date.year(),
            Self::Instant { at } => return at.as_millisecond(),
            Self::Unknown { .. } => return UNANCHORED_MS,
        };
        if year > 0 {
            Timestamp::MAX.as_millisecond()
        } else {
            Timestamp::MIN.as_millisecond()
        }
    }
}

/// [`UNANCHORED_MS`] as a timestamp. It is in range by construction.
fn last() -> Timestamp {
    Timestamp::from_millisecond(UNANCHORED_MS).unwrap_or(Timestamp::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unknown::Unknowns;
    use jiff::civil;

    fn utc() -> TimeZone {
        TimeZone::UTC
    }

    /// The bug ADR-0066 §3 names: a value with no instant used to resolve
    /// to the Unix epoch, which reads as fifty years overdue.
    #[test]
    fn a_civil_time_past_the_timestamp_range_is_an_error_not_the_epoch() {
        let edge = civil::date(9999, 12, 31).at(23, 0, 0, 0);
        let honolulu = TimeZone::get("Pacific/Honolulu").unwrap();
        for t in [
            SunriseTime::floating(edge),
            SunriseTime::zoned(edge, "Pacific/Honolulu"),
            SunriseTime::all_day(civil::date(9999, 12, 31)),
        ] {
            let got = t.to_instant(&honolulu);
            assert!(
                matches!(got, Err(Unresolvable::OutOfRange { .. })),
                "{t}: {got:?}"
            );
            assert_eq!(t.resolve_in(&honolulu), None);
        }
        let kiritimati = TimeZone::get("Pacific/Kiritimati").unwrap();
        // The storage key keeps it at the end of the range, not in 1970.
        let far = SunriseTime::floating(edge);
        assert_eq!(far.index_ms(), Timestamp::MAX.as_millisecond());
        let earliest = SunriseTime::floating(civil::DateTime::MIN);
        assert!(earliest.to_instant(&kiritimati).is_err());
        assert_eq!(earliest.index_ms(), Timestamp::MIN.as_millisecond());
    }

    #[test]
    fn an_unknown_kind_with_no_anchor_is_unanchored() {
        let t = SunriseTime::Unknown {
            kind: "lunar".into(),
            raw: Unknowns::new(),
        };
        assert_eq!(
            t.to_instant(&utc()),
            Err(Unresolvable::UnknownKind {
                kind: "lunar".into()
            })
        );
        assert_eq!(t.instant_or_last(&utc()).as_millisecond(), UNANCHORED_MS);
        assert_eq!(t.index_ms(), UNANCHORED_MS);
    }

    /// A sort places what it cannot resolve last, and everything else at its
    /// instant.
    #[test]
    fn instant_or_last_is_the_instant_when_there_is_one() {
        let t = SunriseTime::floating(civil::date(2026, 3, 8).at(9, 0, 0, 0));
        assert_eq!(t.instant_or_last(&utc()), t.to_instant(&utc()).unwrap());
        let edge = SunriseTime::floating(civil::date(9999, 12, 31).at(23, 0, 0, 0));
        assert!(t.instant_or_last(&utc()) < edge.instant_or_last(&utc()));
    }

    /// An unknown zone name resolves in the reader's zone, is flagged, and is
    /// not rewritten.
    #[test]
    fn a_zone_this_build_does_not_know_is_flagged_and_kept() {
        let civil = civil::date(2026, 6, 1).at(9, 0, 0, 0);
        let t = SunriseTime::zoned(civil, "Mars/Olympus_Mons");
        assert!(t.zone_unknown());
        assert!(!SunriseTime::zoned(civil, "Asia/Kathmandu").zone_unknown());
        assert!(!SunriseTime::floating(civil).zone_unknown());
        let kolkata = TimeZone::get("Asia/Kolkata").unwrap();
        assert_eq!(
            t.to_instant(&kolkata).unwrap(),
            SunriseTime::floating(civil).to_instant(&kolkata).unwrap()
        );
        assert_eq!(t.to_string(), "2026-06-01T09:00:00[Mars/Olympus_Mons]");
    }
}

//! `SunriseTime` — the four kinds of time a task manager actually has to hold.
//!
//! Issue #6. Every scheduled field in v1 was a bare `jiff::Timestamp`: a UTC
//! instant. That is exactly one of the four things a user means, and it
//! silently corrupts the other three:
//!
//! | The user says | A UTC instant records | What breaks |
//! |---|---|---|
//! | "the 09:00 standup" | the instant 09:00 was in *some* zone | fly to Berlin and the standup moves |
//! | "09:00 New York, whatever I'm doing" | an instant | correct until the DST rule for that date changes |
//! | "sometime Tuesday morning" | 09:00 UTC on Tuesday | a Tuesday-morning task shows up Monday evening |
//! | "my birthday, the 4th" | midnight UTC on the 4th | west of UTC the birthday lands on the 3rd |
//!
//! So the kind is part of the value, not a convention:
//!
//! * [`SunriseTime::Instant`] — a fixed point on the timeline. "The meeting
//!   starts now." Never moves.
//! * [`SunriseTime::Zoned`] — a civil time pinned to a named zone. "09:00 in
//!   `America/New_York`." The instant is *derived*, so it stays correct across
//!   a DST rule change; the zone is stored because the offset is not a fact
//!   about the value, it is a fact about the zone on that date.
//! * [`SunriseTime::Floating`] — a civil time with no zone at all. "09:00,
//!   wherever I am." Resolves against the reading device's zone, which is the
//!   whole point: it follows the user.
//! * [`SunriseTime::AllDay`] — a date with no time. "The 4th." Has no instant
//!   until someone picks a zone, and even then only as a half-open day.
//!
//! `ScheduleConstraint` already models civil windows with `jiff`'s civil types
//! (`crate::constraint`), and ADR-0011 makes `jiff` the workspace's time
//! library; this follows both.
//!
//! ## Storage
//!
//! Every kind projects onto ONE `INTEGER` index column plus two sidecars, so
//! existing range queries (`due_at_ms < ?`) and the FTS projection keep working
//! unchanged while the kind survives a round trip. See [`SunriseTime::to_parts`]
//! and [`SunriseTime::from_parts`].

use jiff::civil;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// A time value that knows what kind of time it is.
///
/// Ordering is by [`SunriseTime::index_ms`] — the same key storage indexes on —
/// so a sorted list of mixed kinds matches a `ORDER BY *_at_ms` from SQL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SunriseTime {
    /// A fixed point on the timeline, zone-independent.
    Instant {
        /// The instant.
        at: Timestamp,
    },
    /// A civil time pinned to a named IANA zone.
    Zoned {
        /// Wall-clock date and time in `tz`.
        civil: civil::DateTime,
        /// IANA zone name, e.g. `"America/New_York"`.
        tz: String,
    },
    /// A civil time with no zone: resolves against the reading device.
    Floating {
        /// Wall-clock date and time.
        civil: civil::DateTime,
    },
    /// A whole date with no time of day.
    AllDay {
        /// The date.
        date: civil::Date,
    },
}

/// The `_kind` sidecar values. Stable strings: they are persisted.
pub mod kind {
    /// [`super::SunriseTime::Instant`].
    pub const INSTANT: &str = "instant";
    /// [`super::SunriseTime::Zoned`].
    pub const ZONED: &str = "zoned";
    /// [`super::SunriseTime::Floating`].
    pub const FLOATING: &str = "floating";
    /// [`super::SunriseTime::AllDay`].
    pub const ALL_DAY: &str = "all_day";
}

impl SunriseTime {
    /// A fixed instant.
    #[must_use]
    pub const fn instant(at: Timestamp) -> Self {
        Self::Instant { at }
    }

    /// A civil time in a named zone.
    #[must_use]
    pub fn zoned(civil: civil::DateTime, tz: impl Into<String>) -> Self {
        Self::Zoned {
            civil,
            tz: tz.into(),
        }
    }

    /// A civil time with no zone.
    #[must_use]
    pub const fn floating(civil: civil::DateTime) -> Self {
        Self::Floating { civil }
    }

    /// A whole date.
    #[must_use]
    pub const fn all_day(date: civil::Date) -> Self {
        Self::AllDay { date }
    }

    /// The `_kind` sidecar for this value.
    #[must_use]
    pub const fn kind_str(&self) -> &'static str {
        match self {
            Self::Instant { .. } => kind::INSTANT,
            Self::Zoned { .. } => kind::ZONED,
            Self::Floating { .. } => kind::FLOATING,
            Self::AllDay { .. } => kind::ALL_DAY,
        }
    }

    /// Resolve to a real instant, using `tz` for the kinds that carry no zone
    /// of their own.
    ///
    /// * `Instant` ignores `tz` — it already is one.
    /// * `Zoned` ignores `tz` and uses its OWN zone; that is what makes
    ///   "09:00 New York" survive being read in Berlin.
    /// * `Floating` and `AllDay` resolve in `tz`, which is the reading
    ///   device's zone. `AllDay` resolves to the START of the day.
    ///
    /// An unresolvable zone name, or a civil time that does not exist in it (a
    /// spring-forward gap), falls back to the compatible resolution `jiff`
    /// offers rather than failing: a scheduled time that cannot be rendered is
    /// worse than one rendered an hour off, and the stored civil value is
    /// unchanged either way.
    #[must_use]
    pub fn to_instant(&self, tz: &TimeZone) -> Timestamp {
        match self {
            Self::Instant { at } => *at,
            Self::Zoned { civil, tz: name } => {
                let zone = TimeZone::get(name).unwrap_or_else(|_| tz.clone());
                zone.to_zoned(*civil)
                    .map_or_else(|_| Timestamp::UNIX_EPOCH, |z| z.timestamp())
            }
            Self::Floating { civil } => tz
                .to_zoned(*civil)
                .map_or_else(|_| Timestamp::UNIX_EPOCH, |z| z.timestamp()),
            Self::AllDay { date } => tz
                .to_zoned(date.to_datetime(civil::Time::midnight()))
                .map_or_else(|_| Timestamp::UNIX_EPOCH, |z| z.timestamp()),
        }
    }

    /// The epoch-millisecond key this value is INDEXED under.
    ///
    /// Not the same thing as [`SunriseTime::to_instant`], and deliberately so.
    /// `Instant` and `Zoned` index at their true instant — both know one.
    /// `Floating` and `AllDay` have no instant until a reader supplies a zone,
    /// but a database index cannot wait for a reader, so they are anchored in
    /// UTC. Two consequences worth stating out loud:
    ///
    /// * The key is **stable**: it does not change when the device's zone
    ///   changes, so an index built on one device is valid on another.
    /// * The key is **lossless**: `from_parts` reconstructs the exact civil
    ///   value from it, because anchoring in UTC is a bijection.
    ///
    /// It is within a day of the true instant for any real zone, which is what
    /// makes it a usable coarse filter. Anything that must be exact resolves
    /// with `to_instant` after reading.
    #[must_use]
    pub fn index_ms(&self) -> i64 {
        match self {
            Self::Instant { at } => at.as_millisecond(),
            Self::Zoned { .. } | Self::Floating { .. } | Self::AllDay { .. } => {
                self.to_instant(&TimeZone::UTC).as_millisecond()
            }
        }
    }

    /// Project to the three storage columns: `(index_ms, kind, tz)`.
    ///
    /// The `tz` sidecar is `Some` only for [`SunriseTime::Zoned`]; the other
    /// kinds either carry no zone or are defined by not having one.
    #[must_use]
    pub fn to_parts(&self) -> (i64, &'static str, Option<&str>) {
        let tz = match self {
            Self::Zoned { tz, .. } => Some(tz.as_str()),
            _ => None,
        };
        (self.index_ms(), self.kind_str(), tz)
    }

    /// Rebuild from the three storage columns.
    ///
    /// An unrecognised `kind` degrades to [`SunriseTime::Instant`] rather than
    /// failing: a row written by a newer build that added a fifth kind is still
    /// a real time at `index_ms`, and refusing to read it would lose the whole
    /// task over one column. Same rule as every other lossy enum decode in the
    /// domain.
    #[must_use]
    pub fn from_parts(index_ms: i64, kind: &str, tz: Option<&str>) -> Self {
        let at = Timestamp::from_millisecond(index_ms).unwrap_or(Timestamp::UNIX_EPOCH);
        let utc_civil = || at.to_zoned(TimeZone::UTC).datetime();
        match kind {
            kind::ZONED => {
                // A zoned value's index key IS its true instant, so the civil
                // time comes back by converting into its OWN zone — not UTC.
                let name = tz.unwrap_or("UTC");
                let zone = TimeZone::get(name).unwrap_or(TimeZone::UTC);
                Self::Zoned {
                    civil: at.to_zoned(zone).datetime(),
                    tz: name.to_string(),
                }
            }
            kind::FLOATING => Self::Floating { civil: utc_civil() },
            kind::ALL_DAY => Self::AllDay {
                date: utc_civil().date(),
            },
            _ => Self::Instant { at },
        }
    }

    /// Does this value denote a whole day rather than a moment in one?
    #[must_use]
    pub const fn is_all_day(&self) -> bool {
        matches!(self, Self::AllDay { .. })
    }
}

/// The tagged wire form, and the pre-`SunriseTime` form it replaced.
///
/// `Deserialize` is hand-rolled rather than derived so a payload written at
/// `DOC_SCHEMA_V = 1` — where these fields were a bare RFC 3339 instant —
/// still reads, as an [`SunriseTime::Instant`]. That is the promise
/// `docs/10-cross-cutting/protocol-versioning.md` makes about minor schema
/// changes, and the cheapest possible place to keep it.
#[derive(Deserialize)]
#[serde(untagged)]
enum SunriseTimeRepr {
    Tagged(Tagged),
    /// `DOC_SCHEMA_V = 1`: a bare instant, no kind.
    Legacy(Timestamp),
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Tagged {
    Instant { at: Timestamp },
    Zoned { civil: civil::DateTime, tz: String },
    Floating { civil: civil::DateTime },
    AllDay { date: civil::Date },
}

impl<'de> Deserialize<'de> for SunriseTime {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match SunriseTimeRepr::deserialize(d)? {
            SunriseTimeRepr::Tagged(Tagged::Instant { at }) | SunriseTimeRepr::Legacy(at) => {
                Self::Instant { at }
            }
            SunriseTimeRepr::Tagged(Tagged::Zoned { civil, tz }) => Self::Zoned { civil, tz },
            SunriseTimeRepr::Tagged(Tagged::Floating { civil }) => Self::Floating { civil },
            SunriseTimeRepr::Tagged(Tagged::AllDay { date }) => Self::AllDay { date },
        })
    }
}

impl std::fmt::Display for SunriseTime {
    /// A round-trippable rendering that shows the KIND, because a rendering
    /// that hides it is how "sometime Tuesday" became "Monday 20:00" in the
    /// first place. Zone-less kinds print no offset, precisely because they
    /// have none.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Instant { at } => write!(f, "{at}"),
            Self::Zoned { civil, tz } => write!(f, "{civil}[{tz}]"),
            Self::Floating { civil } => write!(f, "{civil}"),
            Self::AllDay { date } => write!(f, "{date}"),
        }
    }
}

impl From<Timestamp> for SunriseTime {
    fn from(at: Timestamp) -> Self {
        Self::Instant { at }
    }
}

impl PartialOrd for SunriseTime {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SunriseTime {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Index order, then a total tiebreak so `Ord` stays consistent with
        // `Eq`: two different values must never compare Equal.
        self.index_ms()
            .cmp(&other.index_ms())
            .then_with(|| self.kind_str().cmp(other.kind_str()))
            .then_with(|| match (self, other) {
                (Self::Zoned { tz: a, .. }, Self::Zoned { tz: b, .. }) => a.cmp(b),
                _ => std::cmp::Ordering::Equal,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ny() -> TimeZone {
        TimeZone::get("America/New_York").unwrap()
    }

    #[test]
    fn an_instant_is_the_same_moment_in_every_zone() {
        let t = SunriseTime::instant(Timestamp::from_millisecond(1_700_000_000_000).unwrap());
        assert_eq!(t.to_instant(&TimeZone::UTC), t.to_instant(&ny()));
    }

    /// The point of `Zoned`: the zone travels with the value.
    #[test]
    fn a_zoned_time_ignores_the_reading_device() {
        let t = SunriseTime::zoned(civil::date(2024, 6, 1).at(9, 0, 0, 0), "America/New_York");
        let berlin = TimeZone::get("Europe/Berlin").unwrap();
        assert_eq!(t.to_instant(&berlin), t.to_instant(&TimeZone::UTC));
        // 09:00 EDT on 2024-06-01 is 13:00 UTC.
        assert_eq!(
            t.to_instant(&TimeZone::UTC)
                .to_zoned(TimeZone::UTC)
                .datetime()
                .hour(),
            13
        );
    }

    /// ...and the point of `Floating`: it follows the reader instead.
    #[test]
    fn a_floating_time_follows_the_device_zone() {
        let t = SunriseTime::floating(civil::date(2024, 6, 1).at(9, 0, 0, 0));
        assert_ne!(t.to_instant(&TimeZone::UTC), t.to_instant(&ny()));
        for zone in [TimeZone::UTC, ny()] {
            assert_eq!(
                t.to_instant(&zone).to_zoned(zone.clone()).datetime().hour(),
                9,
                "a floating 09:00 is 09:00 wherever it is read"
            );
        }
    }

    /// The bug the AllDay kind exists to prevent: a birthday on the 4th
    /// showing as the 3rd for everyone west of UTC.
    #[test]
    fn an_all_day_date_is_that_date_in_the_reading_zone() {
        let t = SunriseTime::all_day(civil::date(2024, 7, 4));
        for zone in [
            TimeZone::UTC,
            ny(),
            TimeZone::get("Pacific/Auckland").unwrap(),
        ] {
            let d = t.to_instant(&zone).to_zoned(zone.clone()).date();
            assert_eq!(d, civil::date(2024, 7, 4), "in {zone:?}");
        }
    }

    #[test]
    fn every_kind_round_trips_through_storage() {
        let cases = [
            SunriseTime::instant(Timestamp::from_millisecond(1_700_000_000_123).unwrap()),
            SunriseTime::zoned(civil::date(2024, 6, 1).at(9, 30, 0, 0), "America/New_York"),
            SunriseTime::floating(civil::date(2024, 6, 1).at(9, 30, 0, 0)),
            SunriseTime::all_day(civil::date(2024, 7, 4)),
        ];
        for c in cases {
            let (ms, k, tz) = c.to_parts();
            assert_eq!(SunriseTime::from_parts(ms, k, tz), c, "round trip {c:?}");
        }
    }

    /// The index key must not move when the device's zone does — an index
    /// built on one device has to stay valid on another.
    #[test]
    fn the_index_key_is_independent_of_the_device_zone() {
        let t = SunriseTime::floating(civil::date(2024, 6, 1).at(9, 30, 0, 0));
        let before = t.index_ms();
        // `index_ms` takes no zone argument at all; this asserts the contract
        // rather than the implementation, by checking the value equals the UTC
        // resolution and nothing else.
        assert_eq!(before, t.to_instant(&TimeZone::UTC).as_millisecond());
    }

    #[test]
    fn an_unknown_kind_degrades_to_an_instant() {
        let t = SunriseTime::from_parts(1_700_000_000_000, "lunar_phase", None);
        assert_eq!(
            t,
            SunriseTime::instant(Timestamp::from_millisecond(1_700_000_000_000).unwrap())
        );
    }

    #[test]
    fn ordering_matches_the_storage_index() {
        let mut v = [
            SunriseTime::all_day(civil::date(2024, 7, 4)),
            SunriseTime::instant(Timestamp::from_millisecond(0).unwrap()),
            SunriseTime::floating(civil::date(2024, 6, 1).at(9, 30, 0, 0)),
        ];
        v.sort();
        let keys: Vec<i64> = v.iter().map(SunriseTime::index_ms).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn display_shows_the_kind() {
        assert_eq!(
            SunriseTime::zoned(civil::date(2024, 6, 1).at(9, 0, 0, 0), "America/New_York")
                .to_string(),
            "2024-06-01T09:00:00[America/New_York]"
        );
        assert_eq!(
            SunriseTime::floating(civil::date(2024, 6, 1).at(9, 0, 0, 0)).to_string(),
            "2024-06-01T09:00:00"
        );
        assert_eq!(
            SunriseTime::all_day(civil::date(2024, 7, 4)).to_string(),
            "2024-07-04"
        );
    }

    /// A payload written at `DOC_SCHEMA_V = 1`, where the field was a bare
    /// instant with no kind, still reads.
    #[test]
    fn a_pre_schema_bare_instant_still_deserializes() {
        let legacy = sunrise_cbor::encode_canonical(
            &Timestamp::from_millisecond(1_762_680_600_789).unwrap(),
        )
        .expect("encode legacy");
        let t: SunriseTime = ciborium::de::from_reader(&legacy[..]).expect("legacy decodes");
        assert_eq!(
            t,
            SunriseTime::instant(Timestamp::from_millisecond(1_762_680_600_789).unwrap())
        );
    }

    #[test]
    fn cbor_round_trips_every_kind() {
        for c in [
            SunriseTime::instant(Timestamp::from_millisecond(1).unwrap()),
            SunriseTime::zoned(civil::date(2024, 6, 1).at(9, 0, 0, 0), "Europe/Berlin"),
            SunriseTime::floating(civil::date(2024, 6, 1).at(9, 0, 0, 0)),
            SunriseTime::all_day(civil::date(2024, 7, 4)),
        ] {
            let bytes = sunrise_cbor::encode_canonical(&c).expect("encode");
            let back: SunriseTime = sunrise_cbor::decode_canonical(&bytes).expect("decode");
            assert_eq!(back, c);
        }
    }
}

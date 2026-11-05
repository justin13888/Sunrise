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

mod codec;
mod resolve;
mod shift;
mod week;
mod zone;

pub use codec::compat;
pub use resolve::Unresolvable;
pub use shift::shift_civil_days;
pub use week::first_day_of_week;
pub use zone::same_zone;

use crate::unknown::Unknowns;
use codec::{decode_raw_sidecar, encode_raw_sidecar};
use jiff::civil;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use std::borrow::Cow;

/// A time value that knows what kind of time it is.
///
/// [`Ord`] is by [`SunriseTime::index_ms`] — the same key storage indexes on —
/// so a sorted list of mixed kinds matches a `ORDER BY *_at_ms` from SQL. That
/// order is **storage's**, and only storage's: `index_ms` anchors the zone-less
/// kinds in UTC, up to 14 hours from where any real reader puts them
/// (`docs/10-cross-cutting/time.md` §2). A domain decision — a validator, a
/// view, lateness, overlap, a sort a user sees — compares with
/// [`SunriseTime::cmp_in`] in the reader's zone instead.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// A kind this build does not know, kept verbatim (ADR-0045 §6).
    ///
    /// A newer build added it. This build cannot resolve it, so it never
    /// fails the op that carries it and it writes it back byte for byte. It
    /// is placed on the timeline only when `raw` carries an `at` instant
    /// (see [`SunriseTime::index_ms`]).
    Unknown {
        /// The `kind` string exactly as it arrived.
        kind: String,
        /// Every other field of the value, verbatim.
        raw: Unknowns,
    },
}

/// A [`SunriseTime`] in a field that was a bare instant before
/// `DOC_SCHEMA_V` 15, written through [`compat`]: an instant as the bare
/// timestamp, every other kind tagged.
///
/// The same Rust type; the name is what the entity registry and the
/// canonical document schema call such a field, so the encoding is part of
/// the schema's fingerprint.
pub type CompatTime = SunriseTime;

/// The key an [`SunriseTime::Unknown`] value with no `at` instant is indexed
/// at, and the instant [`SunriseTime::instant_or_last`] sorts a value that
/// cannot be placed at: `9999-01-01T00:00:00Z`.
///
/// Late rather than early, so a value this build cannot place sorts after
/// every value it can, and never reads as overdue or already past. A year
/// short of `jiff`'s maximum, so arithmetic on the resolved instant does not
/// overflow.
pub const UNANCHORED_MS: i64 = 253_370_764_800_000;

/// How far a query that selects by time widens its `index_ms` bounds on each
/// side before the exact filter runs: 48 hours.
///
/// `docs/10-cross-cutting/time.md` §2 rule 2. `index_ms` puts a floating or
/// all-day value up to 14 hours from where a reader in any real zone resolves
/// it (UTC−12 to UTC+14), and an all-day value occupies a whole planner day
/// that can extend up to 24 hours past its start; 38 hours rounded up. A row
/// the prefilter admits is then kept or dropped on its resolved instant, so
/// the slack decides only how much is read, never what is returned.
pub const PREFILTER_SLACK_MS: i64 = 48 * 60 * 60 * 1000;

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

    /// Every kind this build knows. Any other `kind` is
    /// [`super::SunriseTime::Unknown`].
    pub const KNOWN: [&str; 4] = [INSTANT, ZONED, FLOATING, ALL_DAY];
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

    /// The `_kind` sidecar for this value; an unknown kind's raw string.
    #[must_use]
    pub fn kind_str(&self) -> &str {
        match self {
            Self::Instant { .. } => kind::INSTANT,
            Self::Zoned { .. } => kind::ZONED,
            Self::Floating { .. } => kind::FLOATING,
            Self::AllDay { .. } => kind::ALL_DAY,
            Self::Unknown { kind, .. } => kind,
        }
    }

    /// Whether this is a kind this build does not know.
    #[must_use]
    pub const fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown { .. })
    }

    /// The instant an unknown kind carries in its `at` field, if it carries
    /// one in the wire form of a timestamp. `None` for every known kind.
    ///
    /// `at` because it is the field the one kind that IS an instant spells
    /// its instant with, and a newer kind that is anchored on the timeline
    /// has no better name for it.
    #[must_use]
    pub fn unknown_anchor(&self) -> Option<Timestamp> {
        match self {
            Self::Unknown { raw, .. } => raw
                .get("at")
                .and_then(|v| v.get().as_text())
                .and_then(|s| s.parse::<Timestamp>().ok()),
            _ => None,
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
    /// A civil time resolves by the one DST rule in
    /// `docs/10-cross-cutting/time.md` §3, jiff's `Compatible`
    /// disambiguation: a time inside a spring-forward gap moves forward by the
    /// length of the gap, and a time inside a fall-back fold takes the earlier
    /// of its two instants. That is the defined answer, not a fallback. A
    /// `Zoned` value naming a zone this build does not know resolves in `tz`
    /// and reports [`SunriseTime::zone_unknown`]; its stored value is
    /// unchanged.
    ///
    /// # Errors
    ///
    /// [`Unresolvable`] when there is no instant to give, and never a
    /// stand-in (`docs/11-adr/0066-times-survive-travel.md` §3): an unknown
    /// kind that carries no `at` instant, or a civil value at the edge of the
    /// calendar that has no instant in the zone it resolves in. Each caller
    /// decides what that means for its reader: a sort places the value last
    /// ([`SunriseTime::instant_or_last`], [`SunriseTime::cmp_in`]), a
    /// reminder is not scheduled and the plan says why, and a view shows the
    /// stored civil text ([`std::fmt::Display`]).
    pub fn to_instant(&self, tz: &TimeZone) -> Result<Timestamp, Unresolvable> {
        let civil_in = |zone: &TimeZone, dt: civil::DateTime| {
            zone.to_zoned(dt)
                .map(|z| z.timestamp())
                .map_err(|_| Unresolvable::OutOfRange {
                    value: self.to_string(),
                })
        };
        match self {
            Self::Unknown { kind, .. } => self
                .unknown_anchor()
                .ok_or_else(|| Unresolvable::UnknownKind { kind: kind.clone() }),
            Self::Instant { at } => Ok(*at),
            Self::Zoned { civil, tz: name } => {
                let zone = TimeZone::get(name).unwrap_or_else(|_| tz.clone());
                civil_in(&zone, *civil)
            }
            Self::Floating { civil } => civil_in(tz, *civil),
            Self::AllDay { date } => civil_in(tz, date.to_datetime(civil::Time::midnight())),
        }
    }

    /// The instant this value is compared at by a reader in `tz`, or `None`
    /// when [`SunriseTime::to_instant`] has none to give: a value this build
    /// cannot place, which is compared with nothing rather than with a
    /// stand-in, because a stand-in would invent an answer.
    ///
    /// An `AllDay` value resolves to the start of its day.
    /// `docs/10-cross-cutting/time.md` §2 rule 1.
    #[must_use]
    pub fn resolve_in(&self, tz: &TimeZone) -> Option<Timestamp> {
        self.to_instant(tz).ok()
    }

    /// The instant this value is **missed** at, read as a deadline in `tz`:
    /// the end of the day for an `AllDay` value, and
    /// [`SunriseTime::resolve_in`] for every other kind.
    ///
    /// `due()` in ADR-0047 §Due instant, and the one exception
    /// `docs/10-cross-cutting/time.md` §2 rule 1 makes to start-of-day: a task
    /// due "Friday" is on time all of Friday. Here the day is the civil day,
    /// ending at the next civil midnight in `tz`.
    ///
    /// A reader with a day schedule reads deadlines through
    /// [`crate::deadline_at`], which ends an all-day value's day at the end
    /// of its planner day, `boundary(d + 1)`, and reduces to this with an
    /// empty schedule. Lateness, the briefs and the ordering warnings read
    /// that one.
    #[must_use]
    pub fn due_in(&self, tz: &TimeZone) -> Option<Timestamp> {
        match self {
            Self::AllDay { date } => {
                let next = date.tomorrow().ok()?;
                tz.to_zoned(next.to_datetime(civil::Time::midnight()))
                    .ok()
                    .map(|z| z.timestamp())
            }
            _ => self.resolve_in(tz),
        }
    }

    /// The civil date a reader in `tz` sees this value on: its own date for
    /// `AllDay`, and the date of its resolved instant in `tz` for every other
    /// kind. `None` where [`SunriseTime::resolve_in`] is.
    ///
    /// Membership in a day is decided on this, never on an instant window,
    /// because an `AllDay` value occupies its whole day and a timed value
    /// belongs to exactly one.
    #[must_use]
    pub fn day_in(&self, tz: &TimeZone) -> Option<civil::Date> {
        match self {
            Self::AllDay { date } => Some(*date),
            _ => self.resolve_in(tz).map(|at| at.to_zoned(tz.clone()).date()),
        }
    }

    /// The order a reader in `tz` sees two values in
    /// (`docs/10-cross-cutting/time.md` §2 rule 3): by resolved instant, then
    /// `AllDay` before a timed value at the same instant, then the storage
    /// order as a total tiebreak so two different values never compare
    /// `Equal`. A value [`SunriseTime::resolve_in`] cannot place sorts after
    /// every value it can.
    ///
    /// This is the comparison every domain decision uses; [`Ord`] is
    /// storage's.
    #[must_use]
    pub fn cmp_in(&self, other: &Self, tz: &TimeZone) -> std::cmp::Ordering {
        let placed = |t: &Self| t.resolve_in(tz);
        match (placed(self), placed(other)) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| other.is_all_day().cmp(&self.is_all_day()))
        .then_with(|| self.cmp(other))
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
    /// It is up to 14 hours from the true instant in any real zone, so it is a
    /// **storage key and nothing else** (`docs/10-cross-cutting/time.md` §2):
    /// a SQL prefilter widens its bounds by [`PREFILTER_SLACK_MS`] and the
    /// result is decided in Rust with [`SunriseTime::resolve_in`] or
    /// [`SunriseTime::cmp_in`]. No domain decision compares it, and
    /// `tests/index_ms_lint.rs` holds every caller outside storage to that.
    ///
    /// An unknown kind indexes at its `at` instant when it carries one, and at
    /// [`UNANCHORED_MS`] otherwise, so it sorts after every value this build
    /// can place. [`SunriseTime::index_key`] tells the two apart. A civil
    /// value at the calendar's edge, with no instant even in UTC, indexes at
    /// the nearer end of the timestamp range, never at the epoch.
    #[must_use]
    pub fn index_ms(&self) -> i64 {
        match self {
            Self::Instant { at } => at.as_millisecond(),
            Self::Unknown { .. } => self
                .unknown_anchor()
                .map_or(UNANCHORED_MS, Timestamp::as_millisecond),
            Self::Zoned { .. } | Self::Floating { .. } | Self::AllDay { .. } => self
                .to_instant(&TimeZone::UTC)
                .map_or_else(|_| self.edge_index_ms(), Timestamp::as_millisecond),
        }
    }

    /// [`SunriseTime::index_ms`], or `None` for an unknown kind that carries
    /// no `at` instant: a value this build cannot place on the timeline.
    ///
    /// For a check that compares two times, where comparing against the
    /// [`UNANCHORED_MS`] stand-in would invent a violation.
    #[must_use]
    pub fn index_key(&self) -> Option<i64> {
        match self {
            Self::Unknown { .. } => self.unknown_anchor().map(Timestamp::as_millisecond),
            _ => Some(self.index_ms()),
        }
    }

    /// Project to the three storage columns: `(index_ms, kind, tz)`.
    ///
    /// The `tz` sidecar is `Some` for [`SunriseTime::Zoned`], where it is the
    /// zone name, and for [`SunriseTime::Unknown`], where it is the value's
    /// other fields as `cbor:` and their canonical CBOR in hex. The other
    /// kinds either carry no zone or are defined by not having one.
    #[must_use]
    pub fn to_parts(&self) -> (i64, &str, Option<Cow<'_, str>>) {
        let tz = match self {
            Self::Zoned { tz, .. } => Some(Cow::Borrowed(tz.as_str())),
            Self::Unknown { raw, .. } => Some(Cow::Owned(encode_raw_sidecar(raw))),
            _ => None,
        };
        (self.index_ms(), self.kind_str(), tz)
    }

    /// The `_civil` storage column: the wall-clock value a civil kind means,
    /// as its ISO 8601 text, beside the index key and the sidecars
    /// (`docs/11-adr/0066-times-survive-travel.md` §2).
    ///
    /// `Some` for [`SunriseTime::Zoned`] and [`SunriseTime::Floating`] (a
    /// civil date-time, `2026-03-08T02:30:00`) and [`SunriseTime::AllDay`] (a
    /// date, `2026-03-08`); `None` for the kinds that are not civil. Storing
    /// it is what makes [`SunriseTime::from_parts`] read a stored value back
    /// without consulting a zone: a tzdata change cannot move a stored 09:00,
    /// and a zone this build does not know keeps its civil time byte for
    /// byte.
    #[must_use]
    pub fn civil_part(&self) -> Option<String> {
        match self {
            Self::Zoned { civil, .. } | Self::Floating { civil } => Some(civil.to_string()),
            Self::AllDay { date } => Some(date.to_string()),
            Self::Instant { .. } | Self::Unknown { .. } => None,
        }
    }

    /// Rebuild from the storage columns: the index key, the `_kind` and `_tz`
    /// sidecars, and the `_civil` column [`SunriseTime::civil_part`] wrote.
    ///
    /// A civil kind is rebuilt from `civil` and its zone name and nothing
    /// else (`docs/11-adr/0066-times-survive-travel.md` §2): `index_ms` is a
    /// storage key, and is not read back for one. A `Zoned` value naming a
    /// zone this build cannot resolve keeps its civil time and its name.
    ///
    /// A row written before the `_civil` column existed (migration 0041) has
    /// `civil` NULL until the projection is rebuilt from the op log, which the
    /// first open after that migration does. Such a value is derived from
    /// `index_ms` as it was before: a zoned one in its own zone, a floating
    /// or all-day one in UTC. That derivation is the fallback, and nothing a
    /// build after 0041 writes reaches it.
    ///
    /// An unrecognised `kind` reads back as the [`SunriseTime::Unknown`] value
    /// [`SunriseTime::to_parts`] stored, its other fields decoded from the
    /// `tz` sidecar, so a round trip through storage does not degrade it.
    ///
    /// A row with an unrecognised `kind` and no sidecar this build wrote has
    /// nothing to rebuild the value from. It degrades to
    /// [`SunriseTime::Instant`] at `index_ms` rather than failing: that is
    /// still a real time, a well-formed one to re-emit, and refusing to read
    /// it would lose the whole task over one column.
    #[must_use]
    pub fn from_parts(index_ms: i64, kind: &str, tz: Option<&str>, civil: Option<&str>) -> Self {
        let at = Timestamp::from_millisecond(index_ms).unwrap_or(Timestamp::UNIX_EPOCH);
        let utc_civil = || at.to_zoned(TimeZone::UTC).datetime();
        let stored_civil = civil.and_then(|c| c.parse::<civil::DateTime>().ok());
        match kind {
            kind::ZONED => {
                let name = tz.unwrap_or("UTC");
                let civil = stored_civil.unwrap_or_else(|| {
                    // Pre-0041 rows only: a zoned value's index key was its
                    // true instant, so the civil time comes back in its OWN
                    // zone.
                    let zone = TimeZone::get(name).unwrap_or(TimeZone::UTC);
                    at.to_zoned(zone).datetime()
                });
                Self::Zoned {
                    civil,
                    tz: name.to_string(),
                }
            }
            kind::FLOATING => Self::Floating {
                civil: stored_civil.unwrap_or_else(utc_civil),
            },
            kind::ALL_DAY => Self::AllDay {
                date: civil
                    .and_then(|c| c.parse::<civil::Date>().ok())
                    .or_else(|| stored_civil.map(civil::DateTime::date))
                    .unwrap_or_else(|| utc_civil().date()),
            },
            kind::INSTANT => Self::Instant { at },
            other => match tz.and_then(decode_raw_sidecar) {
                Some(raw) => Self::Unknown {
                    kind: other.to_string(),
                    raw,
                },
                None => Self::Instant { at },
            },
        }
    }

    /// Does this value denote a whole day rather than a moment in one?
    #[must_use]
    pub const fn is_all_day(&self) -> bool {
        matches!(self, Self::AllDay { .. })
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
            Self::Unknown { kind, .. } => match self.unknown_anchor() {
                Some(at) => write!(f, "{at}[unknown kind {kind}]"),
                None => write!(f, "[unknown kind {kind}]"),
            },
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

/// Storage order: by [`SunriseTime::index_ms`], the key `ORDER BY *_at_ms`
/// sorts on, then a total tiebreak. Kept so a value can sit in a `BTreeSet` or
/// be sorted the way a SQL scan returns it; it MUST NOT decide anything a user
/// sees (`docs/10-cross-cutting/time.md` §2 rule 3, which amends ADR-0017).
/// Use [`SunriseTime::cmp_in`] for that.
impl Ord for SunriseTime {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Index order, then a total tiebreak so `Ord` stays consistent with
        // `Eq`: two different values must never compare Equal.
        self.index_ms()
            .cmp(&other.index_ms())
            .then_with(|| self.kind_str().cmp(other.kind_str()))
            .then_with(|| match (self, other) {
                (Self::Zoned { tz: a, .. }, Self::Zoned { tz: b, .. }) => a.cmp(b),
                // Same index and kind: the fields decide, compared as the
                // bytes they are stored as.
                (Self::Unknown { raw: a, .. }, Self::Unknown { raw: b, .. }) if a != b => {
                    encode_raw_sidecar(a).cmp(&encode_raw_sidecar(b))
                }
                _ => std::cmp::Ordering::Equal,
            })
    }
}

#[cfg(test)]
mod tests;

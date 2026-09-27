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

use crate::unknown::Unknowns;
use jiff::civil;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// A time value that knows what kind of time it is.
///
/// Ordering is by [`SunriseTime::index_ms`] — the same key storage indexes on —
/// so a sorted list of mixed kinds matches a `ORDER BY *_at_ms` from SQL.
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

/// The key an [`SunriseTime::Unknown`] value with no `at` instant is indexed
/// and resolved at: `9999-01-01T00:00:00Z`.
///
/// Late rather than early, so a value this build cannot place sorts after
/// every value it can, and never reads as overdue or already past. A year
/// short of `jiff`'s maximum, so arithmetic on the resolved instant does not
/// overflow.
pub const UNANCHORED_MS: i64 = 253_370_764_800_000;

/// The prefix of the `_tz` sidecar that holds an [`SunriseTime::Unknown`]
/// value's other fields: this, then their canonical CBOR in lowercase hex.
const RAW_SIDECAR_PREFIX: &str = "cbor:";

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
    /// An unresolvable zone name, or a civil time that does not exist in it (a
    /// spring-forward gap), falls back to the compatible resolution `jiff`
    /// offers rather than failing: a scheduled time that cannot be rendered is
    /// worse than one rendered an hour off, and the stored civil value is
    /// unchanged either way.
    ///
    /// An unknown kind resolves to its `at` instant when it carries one
    /// ([`SunriseTime::unknown_anchor`]), and to [`UNANCHORED_MS`] otherwise.
    #[must_use]
    pub fn to_instant(&self, tz: &TimeZone) -> Timestamp {
        match self {
            Self::Unknown { .. } => self.unknown_anchor().unwrap_or_else(|| {
                Timestamp::from_millisecond(UNANCHORED_MS).unwrap_or(Timestamp::UNIX_EPOCH)
            }),
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
    ///
    /// An unknown kind indexes at its `at` instant when it carries one, and at
    /// [`UNANCHORED_MS`] otherwise, so it sorts after every value this build
    /// can place. [`SunriseTime::index_key`] tells the two apart.
    #[must_use]
    pub fn index_ms(&self) -> i64 {
        match self {
            Self::Instant { at } => at.as_millisecond(),
            Self::Zoned { .. }
            | Self::Floating { .. }
            | Self::AllDay { .. }
            | Self::Unknown { .. } => self.to_instant(&TimeZone::UTC).as_millisecond(),
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

    /// Rebuild from the three storage columns.
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

/// The tagged wire form, and the pre-`SunriseTime` form it replaced.
///
/// `Deserialize` is hand-rolled rather than derived so a payload written at
/// `DOC_SCHEMA_V = 1` — where these fields were a bare RFC 3339 instant —
/// still reads, as an [`SunriseTime::Instant`]. That is the promise
/// `docs/10-cross-cutting/protocol-versioning.md` makes about minor schema
/// changes, and the cheapest possible place to keep it.
///
/// The third form is a kind this build does not know. It is tried last, so a
/// value of a known kind always decodes as that kind; a known kind whose
/// fields do not parse is refused rather than kept as unknown, because a
/// build that knows the kind would read the same bytes as an error too.
///
/// `untagged` buffers the value once and each form reads that buffer, so the
/// cost of a peer's value is linear in its size.
#[derive(Deserialize)]
#[serde(untagged)]
enum SunriseTimeRepr {
    Tagged(Tagged),
    /// `DOC_SCHEMA_V = 1`: a bare instant, no kind.
    Legacy(Timestamp),
    /// A kind this build does not know.
    Unknown(UnknownTagged),
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Tagged {
    Instant { at: Timestamp },
    Zoned { civil: civil::DateTime, tz: String },
    Floating { civil: civil::DateTime },
    AllDay { date: civil::Date },
}

#[derive(Deserialize)]
struct UnknownTagged {
    kind: String,
    #[serde(flatten)]
    raw: Unknowns,
}

/// The wire form of the four known kinds, borrowed. Derived, so the known
/// kinds encode exactly as they did before `Unknown` existed.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TaggedRef<'a> {
    Instant {
        at: &'a Timestamp,
    },
    Zoned {
        civil: &'a civil::DateTime,
        tz: &'a str,
    },
    Floating {
        civil: &'a civil::DateTime,
    },
    AllDay {
        date: &'a civil::Date,
    },
}

impl Serialize for SunriseTime {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Instant { at } => TaggedRef::Instant { at }.serialize(s),
            Self::Zoned { civil, tz } => TaggedRef::Zoned { civil, tz }.serialize(s),
            Self::Floating { civil } => TaggedRef::Floating { civil }.serialize(s),
            Self::AllDay { date } => TaggedRef::AllDay { date }.serialize(s),
            Self::Unknown { kind, raw } => {
                // The decoder never puts `kind` in `raw`; a value built by hand
                // could, and a map must not carry the key twice.
                let fields = raw.iter().filter(|(k, _)| k.as_str() != "kind");
                let mut m = s.serialize_map(Some(1 + fields.clone().count()))?;
                m.serialize_entry("kind", kind)?;
                for (k, v) in fields {
                    m.serialize_entry(k, v)?;
                }
                m.end()
            }
        }
    }
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
            SunriseTimeRepr::Unknown(UnknownTagged { kind, raw }) => {
                if kind::KNOWN.contains(&kind.as_str()) {
                    return Err(serde::de::Error::custom(format!("malformed `{kind}` time")));
                }
                Self::Unknown { kind, raw }
            }
        })
    }
}

/// The `_tz` sidecar of an [`SunriseTime::Unknown`] value: its other fields'
/// canonical CBOR, hex-encoded after [`RAW_SIDECAR_PREFIX`].
fn encode_raw_sidecar(raw: &Unknowns) -> String {
    // A map of `CborValue`s holds no float and nothing else the canonical
    // encoder refuses, so this does not fail; an empty body is the fallback
    // a failure would read back as.
    let bytes = sunrise_cbor::encode_canonical(raw).unwrap_or_default();
    let mut s = String::with_capacity(RAW_SIDECAR_PREFIX.len() + bytes.len() * 2);
    s.push_str(RAW_SIDECAR_PREFIX);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The inverse of [`encode_raw_sidecar`]; `None` for a sidecar it did not
/// write.
fn decode_raw_sidecar(s: &str) -> Option<Unknowns> {
    let hex = s.strip_prefix(RAW_SIDECAR_PREFIX)?.as_bytes();
    if hex.len() % 2 != 0 {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let bytes = hex
        .chunks_exact(2)
        .map(|p| Some((nibble(p[0])? << 4) | nibble(p[1])?))
        .collect::<Option<Vec<u8>>>()?;
    if bytes.is_empty() {
        return Some(Unknowns::new());
    }
    sunrise_cbor::decode_lenient(&bytes).ok()
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
mod tests {
    use super::*;
    use crate::unknown::CborValue;

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
            lunar(Some("2024-06-01T09:30:00Z")),
            lunar(None),
            SunriseTime::Unknown {
                kind: "bare".into(),
                raw: Unknowns::new(),
            },
        ];
        for c in cases {
            let (ms, k, tz) = c.to_parts();
            assert_eq!(
                SunriseTime::from_parts(ms, k, tz.as_deref()),
                c,
                "round trip {c:?}"
            );
        }
    }

    /// A kind a newer build added, with an optional `at` anchor.
    fn lunar(at: Option<&str>) -> SunriseTime {
        let mut raw = Unknowns::new();
        raw.insert(
            "phase".into(),
            CborValue(ciborium::value::Value::Text("waxing".into())),
        );
        if let Some(at) = at {
            raw.insert(
                "at".into(),
                CborValue(ciborium::value::Value::Text(at.into())),
            );
        }
        SunriseTime::Unknown {
            kind: "lunar".into(),
            raw,
        }
    }

    #[test]
    fn the_unanchored_key_is_the_start_of_9999() {
        assert_eq!(
            Timestamp::from_millisecond(UNANCHORED_MS).unwrap(),
            "9999-01-01T00:00:00Z".parse::<Timestamp>().unwrap()
        );
    }

    /// A kind this build does not know decodes, keeps every field, and writes
    /// back the bytes it arrived as.
    #[test]
    fn an_unknown_kind_round_trips_byte_exact() {
        for t in [lunar(Some("2024-06-01T09:30:00Z")), lunar(None)] {
            let bytes = sunrise_cbor::encode_canonical(&t).unwrap();
            let back: SunriseTime = sunrise_cbor::decode_canonical(&bytes).unwrap();
            assert_eq!(back, t);
            assert!(back.is_unknown());
            assert_eq!(back.kind_str(), "lunar");
            assert_eq!(sunrise_cbor::encode_canonical(&back).unwrap(), bytes);
        }
        let j = serde_json::json!({ "kind": "lunar", "phase": "waxing" });
        let t: SunriseTime = serde_json::from_value(j.clone()).unwrap();
        assert_eq!(t, lunar(None));
        assert_eq!(serde_json::to_value(&t).unwrap(), j);
    }

    /// A known kind whose fields do not parse is still an error: it is not
    /// "a kind this build does not know".
    #[test]
    fn a_malformed_known_kind_is_refused_not_kept() {
        for j in [
            serde_json::json!({ "kind": "instant", "at": "not a time" }),
            serde_json::json!({ "kind": "zoned", "civil": "2024-06-01T09:00:00" }),
        ] {
            assert!(serde_json::from_value::<SunriseTime>(j).is_err());
        }
    }

    /// Placed by its `at` when it has one; after everything else when not.
    #[test]
    fn an_unknown_kind_orders_by_its_anchor_or_last() {
        let anchored = lunar(Some("2024-06-01T09:30:00Z"));
        assert_eq!(
            anchored.index_ms(),
            "2024-06-01T09:30:00Z"
                .parse::<Timestamp>()
                .unwrap()
                .as_millisecond()
        );
        assert_eq!(anchored.index_key(), Some(anchored.index_ms()));

        let unanchored = lunar(None);
        assert_eq!(unanchored.index_ms(), UNANCHORED_MS);
        assert_eq!(unanchored.index_key(), None);

        let mut v = [
            unanchored.clone(),
            SunriseTime::all_day(civil::date(2024, 7, 4)),
            anchored.clone(),
            SunriseTime::instant(Timestamp::from_millisecond(0).unwrap()),
        ];
        v.sort();
        assert_eq!(v[1], anchored);
        assert_eq!(v[3], unanchored);

        // Two unknown values that differ only in a field are not Equal.
        let mut other = lunar(None);
        if let SunriseTime::Unknown { raw, .. } = &mut other {
            raw.insert(
                "phase".into(),
                CborValue(ciborium::value::Value::Text("waning".into())),
            );
        }
        assert_ne!(unanchored.cmp(&other), std::cmp::Ordering::Equal);
    }

    /// A row with an unknown kind and no sidecar this build wrote has nothing
    /// to rebuild from; it degrades to an instant, as before.
    #[test]
    fn an_unknown_kind_without_its_sidecar_degrades_to_an_instant() {
        for tz in [None, Some("Europe/Berlin"), Some("cbor:zz"), Some("cbor:0")] {
            assert_eq!(
                SunriseTime::from_parts(1_700_000_000_000, "lunar_phase", tz),
                SunriseTime::instant(Timestamp::from_millisecond(1_700_000_000_000).unwrap()),
                "sidecar {tz:?}"
            );
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

//! Time: the instant, floating, date and range values a mirror carries.

use sunrise_client_core::vocab::zone_or_utc;
use sunrise_domain::SunriseTime;

use super::{extra_in, extra_out};

/// See [`sunrise_domain::SunriseTime`]: "Tuesday morning", "09:00 New York" and
/// "this instant" are three different answers, and a client that flattens them
/// to one has thrown away what the user meant.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum TimeValue {
    /// A fixed point on the timeline, zone-independent.
    Instant {
        /// The instant.
        at: jiff::Timestamp,
    },
    /// A civil time pinned to a named IANA zone.
    Zoned {
        /// Wall-clock date and time in `tz`.
        civil: jiff::civil::DateTime,
        /// IANA zone name, e.g. `America/New_York`.
        tz: String,
    },
    /// A civil time with no zone: resolves against the reading device.
    Floating {
        /// Wall-clock date and time.
        civil: jiff::civil::DateTime,
    },
    /// A whole date with no time of day.
    AllDay {
        /// The date.
        date: jiff::civil::Date,
    },
    /// A kind this build does not know (ADR-0045 §6). A client renders it as
    /// unknown rather than as a date, and hands it back unchanged to keep it;
    /// [`crate::vocab::time_value_ms`] still places it where the core does.
    Unknown {
        /// The `kind` string exactly as it arrived.
        kind: String,
        /// Every other field of the value, as opaque canonical CBOR. Never
        /// read by a client; only handed back.
        raw: Vec<u8>,
    },
}

impl From<&SunriseTime> for TimeValue {
    fn from(t: &SunriseTime) -> Self {
        match t {
            SunriseTime::Instant { at } => Self::Instant { at: *at },
            SunriseTime::Zoned { civil, tz } => Self::Zoned {
                civil: *civil,
                tz: tz.clone(),
            },
            SunriseTime::Floating { civil } => Self::Floating { civil: *civil },
            SunriseTime::AllDay { date } => Self::AllDay { date: *date },
            SunriseTime::Unknown { kind, raw } => Self::Unknown {
                kind: kind.clone(),
                raw: extra_out(raw).unwrap_or_default(),
            },
        }
    }
}

/// The instant a client built before #506 reads for a routine or recurrence
/// time it still gets as a bare `Timestamp`: the value resolved in zone `tz`
/// (the routine's), or the latest instant for one that has none. The kind is
/// in the matching `*_time` field.
pub(super) fn legacy_instant(t: &SunriseTime, tz: &str) -> jiff::Timestamp {
    t.instant_or_last(&zone_or_utc(tz))
}

impl From<TimeValue> for SunriseTime {
    fn from(t: TimeValue) -> Self {
        match t {
            TimeValue::Instant { at } => Self::Instant { at },
            TimeValue::Zoned { civil, tz } => Self::Zoned { civil, tz },
            TimeValue::Floating { civil } => Self::Floating { civil },
            TimeValue::AllDay { date } => Self::AllDay { date },
            // Empty `raw` is the empty map: `extra_out` sends one as no bytes.
            TimeValue::Unknown { kind, raw } => Self::Unknown {
                kind,
                raw: extra_in(Some(&raw)),
            },
        }
    }
}

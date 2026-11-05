//! Fixtures more than one of `notify`'s test modules reads.

use super::policy::{QuietHours, QuietHoursPolicy};
use jiff::civil;
use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

pub(super) const DAY_MS: i64 = 86_400_000;

pub(super) fn ts(ms: i64) -> Timestamp {
    Timestamp::from_millisecond(ms).unwrap()
}

pub(super) fn quiet(policy: QuietHoursPolicy) -> QuietHours {
    QuietHours {
        start: civil::time(22, 0, 0, 0),
        end: civil::time(7, 0, 0, 0),
        policy,
    }
}

pub(super) fn at_local(hour: i8, minute: i8) -> Zoned {
    civil::date(2026, 3, 4)
        .at(hour, minute, 0, 0)
        .to_zoned(TimeZone::UTC)
        .unwrap()
}

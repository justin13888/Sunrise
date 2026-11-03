//! The `DTSTART` / `DTEND` value grammar: RFC 5545 §3.3.4 `date` and §3.3.5
//! `date-time`, read in the light of their `VALUE` and `TZID` parameters into
//! an [`ICalTime`], and written back out.

use super::lex::param;
use super::ICalTime;
use jiff::{civil, tz::TimeZone};

/// RFC 5545 §3.3.4 `date`: `date-fullyear date-month date-mday`, which is
/// eight ASCII digits and nothing else.
///
/// This is checked before `strptime` rather than left to it, because `jiff`'s
/// `%Y` implements a *superset* of `date-fullyear`: it takes an optional sign
/// and as few as one digit, and the numeric directives after it skip leading
/// whitespace. So `-202 0302` parses, as the year −202. Nothing in iCalendar
/// can express a year outside `0000`–`9999`, which means [`write_time`] cannot
/// render one back — `strftime`'s `%Y` pads to four *columns* including the
/// sign, so year −202 comes out as `-202` and reads back as `-2020`. Refusing
/// the value here is what keeps `write` a fixed point over `parse`: the
/// parser's output range becomes exactly what the writer can spell.
fn is_ical_date(value: &str) -> bool {
    value.len() == 8 && value.bytes().all(|b| b.is_ascii_digit())
}

/// RFC 5545 §3.3.5 `date-time` with its UTC designator already removed: an
/// [`is_ical_date`] `date`, the literal `T`, and `time-hour time-minute
/// time-second` as six more ASCII digits.
fn is_ical_date_time(value: &str) -> bool {
    let Some((date, time)) = value.split_once('T') else {
        return false;
    };
    is_ical_date(date) && time.len() == 6 && time.bytes().all(|b| b.is_ascii_digit())
}

/// Read a `DTSTART` / `DTEND` value in the light of its parameters.
///
/// Returns the value, plus the zone name when a `TZID` had to be abandoned —
/// which the caller reports as a [`super::NoticeCode::UnknownTimezone`] notice
/// (`docs/09-integrations/icalendar.md` §Time zones).
///
/// The value is held to RFC 5545's grammar exactly (see [`is_ical_date`]);
/// anything else is an `Err`, which the caller turns into a
/// [`super::NoticeCode::BadValue`] rather than dropping in silence.
pub(super) fn parse_time(params: &str, value: &str) -> Result<(ICalTime, Option<String>), String> {
    let value = value.trim();
    if param(params, "VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE")) || is_ical_date(value)
    {
        // Reached with a non-DATE value only when `VALUE=DATE` claimed one.
        // That parameter states what the value *is*, so a value that is not
        // one is an error rather than a reason to try the DATE-TIME reading.
        if !is_ical_date(value) {
            return Err("not a DATE; RFC 5545 §3.3.4 wants YYYYMMDD".into());
        }
        let date = civil::Date::strptime("%Y%m%d", value).map_err(|e| e.to_string())?;
        return Ok((ICalTime::Date(date), None));
    }
    // RFC 5545 §3.3.5 form 2: one trailing `Z` states UTC. The spec spells it
    // upper case; lower case is accepted because feeds emit it, and `write`
    // normalises it back to `Z` on the first pass.
    let (naked, utc) = match value.strip_suffix(['Z', 'z']) {
        Some(rest) => (rest, true),
        None => (value, false),
    };
    if !is_ical_date_time(naked) {
        return Err(
            "not a DATE-TIME; RFC 5545 §3.3.5 wants YYYYMMDDTHHMMSS with an optional Z".into(),
        );
    }
    let dt = civil::DateTime::strptime("%Y%m%dT%H%M%S", naked).map_err(|e| e.to_string())?;
    if utc {
        // A `Z` value is UTC by definition, so this conversion cannot be
        // ambiguous and needs no fallback.
        let at = dt
            .to_zoned(TimeZone::UTC)
            .map(|z| z.timestamp())
            .map_err(|e| e.to_string())?;
        return Ok((ICalTime::Utc(at), None));
    }
    match param(params, "TZID") {
        // A zone the bundled tzdb knows: keep the civil time AND the zone, so
        // the value stays "09:00 there" rather than becoming an instant that
        // silently drifts the next time that zone's DST rule changes.
        Some(tz) if TimeZone::get(tz).is_ok() => Ok((
            ICalTime::Zoned {
                civil: dt,
                tz: tz.to_string(),
            },
            None,
        )),
        // Spec'd fallback: UTC, and say so.
        Some(tz) => {
            let at = dt
                .to_zoned(TimeZone::UTC)
                .map(|z| z.timestamp())
                .map_err(|e| e.to_string())?;
            Ok((ICalTime::Utc(at), Some(tz.to_string())))
        }
        // No zone at all is a real iCalendar value, not a missing one: it
        // floats with the reader, which is exactly `SunriseTime::Floating`.
        None => Ok((ICalTime::Floating(dt), None)),
    }
}

/// The parameters and value to write for one time.
///
/// `%Y` renders four digits for every year iCalendar can express, which is the
/// `0000`–`9999` that [`is_ical_date`] is there to hold [`super::parse`] to.
/// Outside that range there is no conforming spelling to emit and `%Y` falls
/// back to a signed, shorter field, so a [`ICalTime`] built by hand from a
/// proleptic year renders something this module's own parser will not read
/// back.
pub(super) fn write_time(t: &ICalTime) -> (String, String) {
    match t {
        ICalTime::Utc(at) => (String::new(), at.strftime("%Y%m%dT%H%M%SZ").to_string()),
        ICalTime::Zoned { civil, tz } => (
            format!(";TZID={tz}"),
            civil.strftime("%Y%m%dT%H%M%S").to_string(),
        ),
        ICalTime::Floating(civil) => (String::new(), civil.strftime("%Y%m%dT%H%M%S").to_string()),
        ICalTime::Date(d) => (";VALUE=DATE".to_string(), d.strftime("%Y%m%d").to_string()),
    }
}

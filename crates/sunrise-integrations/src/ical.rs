//! iCalendar (RFC 5545) reading and writing.
//!
//! This module is the **syntax** layer only: it turns `.ics` text into
//! [`ICalEvent`] values and back, and knows nothing about the vault. The
//! domain mapping lives in [`crate::ical_map`] and the vault driver in
//! [`crate::ical_vault`], so the parser stays testable with no `Core`, no
//! clock and no storage.
//!
//! # What is read
//!
//! * `VEVENT`: `UID`, `SUMMARY`, `DTSTART`, `DTEND`, `DURATION`,
//!   `DESCRIPTION`, `LOCATION`, `RRULE`.
//! * `DTSTART` / `DTEND` keep the distinction RFC 5545 draws between a UTC
//!   instant (`…Z`), a time in a named zone (`TZID=`), a floating local time,
//!   and a whole date (`VALUE=DATE`) — see [`ICalTime`]. Collapsing those four
//!   to one instant is the classic calendar-import bug, and
//!   [`sunrise_domain::SunriseTime`] models exactly the same four cases, so
//!   the distinction survives all the way into the vault.
//! * Line unfolding (RFC 5545 §3.1) and `\\`-escapes (§3.3.11), both
//!   directions, and CRLF or LF line endings on input.
//!
//! # What is not read — and is reported rather than dropped
//!
//! Everything else produces an [`ICalNotice`]. The caller is expected to show
//! them: `docs/09-integrations/icalendar.md` requires that lossy imports are
//! "detected and surfaced", and a parser that drops what it does not model
//! without saying so is how a user discovers, weeks later, that half their
//! calendar is missing.
//!
//! Specifically unmodelled: `VTODO`, `VJOURNAL`, `VFREEBUSY`, `VALARM`,
//! `VTIMEZONE` (a `TZID` is resolved against the bundled IANA tzdb instead of
//! against an inline definition), `RRULE`/`RDATE`/`EXDATE`/`RECURRENCE-ID`,
//! `ATTACH`, `ATTENDEE`, `ORGANIZER`, `CATEGORIES`, `GEO`, `URL`, and any
//! `X-` property.
//!
//! Seven properties are dropped **silently**, listed in [`BOOKKEEPING`]: they
//! are iCalendar's own record-keeping, carry no user content, and Sunrise is
//! not a CalDAV store that has to preserve them byte-for-byte.

use crate::IntegrationError;
use jiff::{civil, tz::TimeZone, Timestamp};

/// Properties that exist to bookkeep an iCalendar object rather than to carry
/// anything a user typed. Dropped without a notice.
pub const BOOKKEEPING: &[&str] = &[
    "DTSTAMP",
    "SEQUENCE",
    "CREATED",
    "LAST-MODIFIED",
    "TRANSP",
    "CLASS",
    "STATUS",
];

/// Components this build reads. Anything else is reported and skipped.
const READ_COMPONENTS: &[&str] = &["VCALENDAR", "VEVENT"];

/// Maximum octets in a content line before RFC 5545 §3.1 requires folding.
const FOLD_AT: usize = 75;

/// A `DTSTART` / `DTEND` value together with what its parameters say it *is*.
///
/// The four cases are RFC 5545's, and they are also
/// [`sunrise_domain::SunriseTime`]'s — which is why an imported calendar keeps
/// meaning what it said. "09:00 in `America/New_York`" is not the same
/// commitment as the instant it happens to resolve to today, and a whole date
/// is not midnight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ICalTime {
    /// `20260301T140000Z` — a fixed instant, stated in UTC.
    Utc(Timestamp),
    /// `TZID=America/New_York:20260301T090000` — civil time in a named zone.
    Zoned {
        /// The wall-clock value.
        civil: civil::DateTime,
        /// The IANA zone name as written in the file.
        tz: String,
    },
    /// `20260301T090000` with no `Z` and no `TZID` — a floating local time.
    Floating(civil::DateTime),
    /// `VALUE=DATE:20260301` — a whole date, no time of day.
    Date(civil::Date),
}

/// One `VEVENT`, in the file's own vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ICalEvent {
    /// `UID` — required; an event without one is skipped.
    pub uid: String,
    /// `SUMMARY` — display title.
    pub summary: Option<String>,
    /// `DTSTART`.
    pub dtstart: Option<ICalTime>,
    /// `DTEND`.
    pub dtend: Option<ICalTime>,
    /// `DURATION`, verbatim (an ISO 8601 duration). Mutually exclusive with
    /// `DTEND` per RFC 5545 §3.6.1; when a file states both, `DTEND` wins and
    /// the duration is reported.
    pub duration: Option<String>,
    /// `DESCRIPTION` — long-form text.
    pub description: Option<String>,
    /// `LOCATION`.
    pub location: Option<String>,
    /// `RRULE`, verbatim.
    pub rrule: Option<String>,
}

impl ICalEvent {
    /// A minimal event: everything but the id left empty.
    #[must_use]
    pub fn new(uid: impl Into<String>) -> Self {
        Self {
            uid: uid.into(),
            summary: None,
            dtstart: None,
            dtend: None,
            duration: None,
            description: None,
            location: None,
            rrule: None,
        }
    }
}

/// Why something in the file did not make it through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeCode {
    /// A whole component this build does not model (`VTODO`, `VALARM`, …).
    UnsupportedComponent,
    /// A property carrying user content that the Block schema cannot hold.
    UnmappedProperty,
    /// A `TZID` that is not in the bundled IANA tzdb.
    UnknownTimezone,
    /// A value that did not parse as the type its property requires.
    BadValue,
    /// The whole item was dropped.
    Skipped,
}

impl NoticeCode {
    /// Stable machine-readable label, for logs and for the FFI seam.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedComponent => "unsupported_component",
            Self::UnmappedProperty => "unmapped_property",
            Self::UnknownTimezone => "unknown_timezone",
            Self::BadValue => "bad_value",
            Self::Skipped => "skipped",
        }
    }
}

/// One thing the caller has to be told about, rather than have silently
/// dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ICalNotice {
    /// What kind of loss this is.
    pub code: NoticeCode,
    /// The `UID` of the event it happened in, when it happened inside one.
    pub uid: Option<String>,
    /// Human-readable detail: the property or component name, and why.
    pub detail: String,
}

impl ICalNotice {
    /// Build a notice.
    fn new(code: NoticeCode, uid: Option<String>, detail: impl Into<String>) -> Self {
        Self {
            code,
            uid,
            detail: detail.into(),
        }
    }
}

/// A parsed document: what was understood, and what was not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Calendar {
    /// One entry per readable `VEVENT`, in file order.
    pub events: Vec<ICalEvent>,
    /// Everything the parse could not carry.
    pub notices: Vec<ICalNotice>,
}

/// Parse an iCalendar document.
///
/// # Errors
///
/// [`IntegrationError::Decode`] when the input contains no `VCALENDAR` at all
/// — that is a file that is not iCalendar, which is worth failing on rather
/// than reporting as an import of zero events.
pub fn parse(input: &str) -> Result<Calendar, IntegrationError> {
    let lines = unfold_lines(input);
    let mut out = Calendar::default();
    let mut saw_calendar = false;
    // Component names, outermost first. The stack is what distinguishes a
    // `DTSTART` in a VEVENT from one in a VTIMEZONE's DAYLIGHT sub-component.
    let mut stack: Vec<String> = Vec::new();
    let mut current: Option<ICalEvent> = None;

    for raw in lines {
        let line = raw.trim_end();
        if line.is_empty() {
            continue;
        }
        let (name, params, value) = split_property(line);
        let upper = name.to_ascii_uppercase();

        if upper == "BEGIN" {
            let comp = value.trim().to_ascii_uppercase();
            if comp == "VCALENDAR" {
                saw_calendar = true;
            }
            if comp == "VEVENT" && stack.last().map(String::as_str) == Some("VCALENDAR") {
                current = Some(ICalEvent::new(String::new()));
            } else if !READ_COMPONENTS.contains(&comp.as_str()) && !inside_unread(&stack) {
                out.notices.push(ICalNotice::new(
                    NoticeCode::UnsupportedComponent,
                    current
                        .as_ref()
                        .map(|e| e.uid.clone())
                        .filter(|u| !u.is_empty()),
                    format!("{comp} is not modelled and was skipped"),
                ));
            }
            stack.push(comp);
            continue;
        }
        if upper == "END" {
            let comp = value.trim().to_ascii_uppercase();
            if comp == "VEVENT" {
                if let Some(ev) = current.take() {
                    if ev.uid.is_empty() {
                        out.notices.push(ICalNotice::new(
                            NoticeCode::Skipped,
                            None,
                            "a VEVENT with no UID has no stable identity and was skipped",
                        ));
                    } else {
                        out.events.push(ev);
                    }
                }
            }
            stack.pop();
            continue;
        }

        // Inside something we do not read (VTIMEZONE, VALARM, VTODO, …). The
        // component itself was already reported; its properties are not
        // reported one by one, which would bury the notice that matters.
        if inside_unread(&stack) {
            continue;
        }
        let Some(ev) = current.as_mut() else {
            // A property directly under VCALENDAR (PRODID, VERSION, CALSCALE,
            // METHOD). Calendar-level bookkeeping; nothing to carry.
            continue;
        };

        read_event_property(ev, &upper, params, value, &mut out.notices);
    }

    if !saw_calendar {
        return Err(IntegrationError::Decode(
            "no BEGIN:VCALENDAR — this is not an iCalendar document".into(),
        ));
    }
    Ok(out)
}

/// Fold one property of an open `VEVENT` into it, or report why it could not
/// be.
///
/// `name` arrives upper-cased. Anything not named here and not in
/// [`BOOKKEEPING`] is reported: an unknown property is exactly the case where
/// silence would lose content nobody knew was there.
fn read_event_property(
    ev: &mut ICalEvent,
    name: &str,
    params: &str,
    value: &str,
    notices: &mut Vec<ICalNotice>,
) {
    let uid = || Some(ev.uid.clone()).filter(|u| !u.is_empty());
    match name {
        "UID" => ev.uid = unescape(value),
        "SUMMARY" => ev.summary = Some(unescape(value)),
        "DESCRIPTION" => ev.description = Some(unescape(value)),
        "LOCATION" => ev.location = Some(unescape(value)),
        "DURATION" => ev.duration = Some(value.trim().to_string()),
        "RRULE" => ev.rrule = Some(value.trim().to_string()),
        "DTSTART" | "DTEND" => match parse_time(params, value) {
            Ok((t, unknown_zone)) => {
                if let Some(zone) = unknown_zone {
                    notices.push(ICalNotice::new(
                        NoticeCode::UnknownTimezone,
                        uid(),
                        format!(
                            "{name} names TZID={zone}, which is not in the IANA database; \
                             read as UTC"
                        ),
                    ));
                }
                if name == "DTSTART" {
                    ev.dtstart = Some(t);
                } else {
                    ev.dtend = Some(t);
                }
            }
            Err(why) => notices.push(ICalNotice::new(
                NoticeCode::BadValue,
                uid(),
                format!("{name}:{value} did not parse ({why})"),
            )),
        },
        other if BOOKKEEPING.contains(&other) => {}
        other => notices.push(ICalNotice::new(
            NoticeCode::UnmappedProperty,
            uid(),
            format!("{other} is not carried by a Block and was dropped"),
        )),
    }
}

/// Render events as an iCalendar document, CRLF-terminated and folded.
#[must_use]
pub fn write(events: &[ICalEvent]) -> String {
    let mut out = String::with_capacity(events.len() * 256);
    push_line(&mut out, "BEGIN", "", "VCALENDAR");
    push_line(&mut out, "VERSION", "", "2.0");
    push_line(&mut out, "PRODID", "", "-//Sunrise//Sunrise v1//EN");
    // Required by RFC 5545 §3.7.1 when absent-means-Gregorian is not assumed;
    // stated so that strict readers (Outlook) do not guess.
    push_line(&mut out, "CALSCALE", "", "GREGORIAN");
    for e in events {
        push_line(&mut out, "BEGIN", "", "VEVENT");
        push_line(&mut out, "UID", "", &escape(&e.uid));
        if let Some(s) = &e.summary {
            push_line(&mut out, "SUMMARY", "", &escape(s));
        }
        if let Some(t) = &e.dtstart {
            let (params, value) = write_time(t);
            push_line(&mut out, "DTSTART", &params, &value);
        }
        if let Some(t) = &e.dtend {
            let (params, value) = write_time(t);
            push_line(&mut out, "DTEND", &params, &value);
        }
        if let Some(d) = &e.duration {
            push_line(&mut out, "DURATION", "", d);
        }
        if let Some(s) = &e.description {
            push_line(&mut out, "DESCRIPTION", "", &escape(s));
        }
        if let Some(s) = &e.location {
            push_line(&mut out, "LOCATION", "", &escape(s));
        }
        if let Some(r) = &e.rrule {
            push_line(&mut out, "RRULE", "", r);
        }
        push_line(&mut out, "END", "", "VEVENT");
    }
    push_line(&mut out, "END", "", "VCALENDAR");
    out
}

/// Whether the innermost open component is one this build does not read.
///
/// Checks the innermost only: a `VALARM` inside a `VEVENT` makes the alarm's
/// properties unread while the event around it stays readable once the alarm
/// closes.
fn inside_unread(stack: &[String]) -> bool {
    stack
        .last()
        .is_some_and(|c| !READ_COMPONENTS.contains(&c.as_str()))
}

/// RFC 5545 §3.1: a line beginning with SPACE or HTAB continues the previous
/// logical line, and the one leading whitespace octet is not part of it.
fn unfold_lines(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in input.lines() {
        if let Some(prev) = out.last_mut() {
            if let Some(rest) = raw.strip_prefix(' ').or_else(|| raw.strip_prefix('\t')) {
                prev.push_str(rest);
                continue;
            }
        }
        out.push(raw.to_string());
    }
    out
}

/// Split `NAME;PARAM=v:value` into its three parts.
///
/// The first `:` outside a quoted parameter value ends the name-and-parameters
/// half. Quoting matters: `TZID="GMT+02:00"` is one parameter whose value
/// contains a colon, and splitting on the first bare `:` would truncate it.
fn split_property(line: &str) -> (&str, &str, &str) {
    let mut in_quotes = false;
    let mut split = line.len();
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => {
                split = i;
                break;
            }
            _ => {}
        }
    }
    let head = &line[..split];
    let value = if split == line.len() {
        ""
    } else {
        &line[split + 1..]
    };
    let name_end = head.find(';').unwrap_or(head.len());
    (&head[..name_end], &head[name_end..], value)
}

/// One parameter's value, unquoted. `params` is the `;`-prefixed remainder
/// from [`split_property`].
fn param<'a>(params: &'a str, want: &str) -> Option<&'a str> {
    for part in params.split(';') {
        // `params` keeps its leading `;`, so the first split is empty, and a
        // malformed parameter with no `=` must be stepped over rather than
        // ending the search.
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        if k.trim().eq_ignore_ascii_case(want) {
            return Some(v.trim().trim_matches('"'));
        }
    }
    None
}

/// Read a `DTSTART` / `DTEND` value in the light of its parameters.
///
/// Returns the value, plus the zone name when a `TZID` had to be abandoned —
/// which the caller reports as `int.import.tz_unknown`
/// (`docs/09-integrations/icalendar.md` §Time zones).
fn parse_time(params: &str, value: &str) -> Result<(ICalTime, Option<String>), String> {
    let value = value.trim();
    if param(params, "VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE")) || value.len() == 8 {
        let date = civil::Date::strptime("%Y%m%d", value).map_err(|e| e.to_string())?;
        return Ok((ICalTime::Date(date), None));
    }
    let utc = value.ends_with('Z') || value.ends_with('z');
    let naked = value.trim_end_matches(['Z', 'z']);
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
fn write_time(t: &ICalTime) -> (String, String) {
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

/// Append one content line, folded per RFC 5545 §3.1.
///
/// Folds on a char boundary at or before 75 octets: a fold that split a
/// multi-byte character would produce two invalid UTF-8 fragments, and the
/// 75-octet limit is a maximum, so stopping short of it is always legal.
fn push_line(out: &mut String, name: &str, params: &str, value: &str) {
    let line = format!("{name}{params}:{value}");
    let bytes = line.as_bytes();
    if bytes.len() <= FOLD_AT {
        out.push_str(&line);
        out.push_str("\r\n");
        return;
    }
    let mut start = 0;
    // The first line may use all 75 octets; a continuation spends one on the
    // leading space.
    let mut budget = FOLD_AT;
    while start < line.len() {
        let mut end = (start + budget).min(line.len());
        while end > start && !line.is_char_boundary(end) {
            end -= 1;
        }
        if start > 0 {
            out.push(' ');
        }
        out.push_str(&line[start..end]);
        out.push_str("\r\n");
        start = end;
        budget = FOLD_AT - 1;
    }
}

/// RFC 5545 §3.3.11 TEXT unescaping.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(',') => out.push(','),
            Some(';') => out.push(';'),
            // An escape of anything else is not defined, and a trailing
            // backslash escapes nothing: keep the backslash rather than eating
            // a character the file did not intend to lose.
            Some('\\') | None => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

/// RFC 5545 §3.3.11 TEXT escaping. Backslash first, or it would double the
/// backslashes the later rules introduce.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace(',', "\\,")
        .replace(';', "\\;")
        .replace('\r', "")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:ev1@example.com\r\n\
SUMMARY:Coffee with Alice\r\n\
DTSTART:20260301T140000Z\r\n\
DTEND:20260301T150000Z\r\n\
DESCRIPTION:Discuss the launch plan\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    fn wrap(body: &str) -> String {
        format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n{body}END:VCALENDAR\r\n")
    }

    fn one(body: &str) -> Calendar {
        parse(&wrap(&format!(
            "BEGIN:VEVENT\r\nUID:ev1\r\n{body}END:VEVENT\r\n"
        )))
        .unwrap()
    }

    #[test]
    fn parse_one_vevent() {
        let cal = parse(SAMPLE).unwrap();
        assert_eq!(cal.events.len(), 1);
        let e = &cal.events[0];
        assert_eq!(e.uid, "ev1@example.com");
        assert_eq!(e.summary.as_deref(), Some("Coffee with Alice"));
        assert!(matches!(e.dtstart, Some(ICalTime::Utc(_))));
        assert!(matches!(e.dtend, Some(ICalTime::Utc(_))));
    }

    #[test]
    fn a_document_with_no_vcalendar_is_refused() {
        assert!(parse("hello, world\r\n").is_err());
    }

    /// The whole reason [`ICalTime`] exists: four spellings, four meanings.
    #[test]
    fn the_four_time_kinds_survive_parsing() {
        let cal = one("DTSTART;TZID=America/New_York:20260301T090000\r\n\
DTEND:20260301T150000Z\r\n");
        assert_eq!(
            cal.events[0].dtstart,
            Some(ICalTime::Zoned {
                civil: civil::date(2026, 3, 1).at(9, 0, 0, 0),
                tz: "America/New_York".into(),
            })
        );

        let floating = one("DTSTART:20260301T090000\r\n");
        assert_eq!(
            floating.events[0].dtstart,
            Some(ICalTime::Floating(civil::date(2026, 3, 1).at(9, 0, 0, 0)))
        );

        let all_day = one("DTSTART;VALUE=DATE:20260301\r\n");
        assert_eq!(
            all_day.events[0].dtstart,
            Some(ICalTime::Date(civil::date(2026, 3, 1)))
        );
    }

    /// A bare 8-digit value is a DATE whether or not `VALUE=DATE` says so;
    /// Outlook omits the parameter.
    #[test]
    fn an_eight_digit_value_reads_as_a_date() {
        let cal = one("DTSTART:20260301\r\n");
        assert_eq!(
            cal.events[0].dtstart,
            Some(ICalTime::Date(civil::date(2026, 3, 1)))
        );
    }

    #[test]
    fn an_unknown_tzid_falls_back_to_utc_and_says_so() {
        let cal = one("DTSTART;TZID=Mars/Olympus:20260301T090000\r\n");
        assert!(matches!(cal.events[0].dtstart, Some(ICalTime::Utc(_))));
        assert_eq!(cal.notices.len(), 1);
        assert_eq!(cal.notices[0].code, NoticeCode::UnknownTimezone);
        assert!(cal.notices[0].detail.contains("Mars/Olympus"));
    }

    /// `TZID="GMT+02:00"` is one parameter containing a colon. Splitting on
    /// the first bare `:` would leave `TZID="GMT+02` and a truncated value.
    #[test]
    fn a_quoted_parameter_may_contain_a_colon() {
        let (name, params, value) = split_property("DTSTART;TZID=\"GMT+02:00\":20260301T090000");
        assert_eq!(name, "DTSTART");
        assert_eq!(params, ";TZID=\"GMT+02:00\"");
        assert_eq!(value, "20260301T090000");
        assert_eq!(param(params, "TZID"), Some("GMT+02:00"));
    }

    #[test]
    fn unmodelled_components_are_reported_not_dropped_in_silence() {
        let cal = parse(&wrap(
            "BEGIN:VTODO\r\nUID:t1\r\nSUMMARY:Buy milk\r\nEND:VTODO\r\n\
BEGIN:VEVENT\r\nUID:ev1\r\nDTSTART:20260301T090000Z\r\n\
BEGIN:VALARM\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT\r\n",
        ))
        .unwrap();
        // The event around the alarm still parses.
        assert_eq!(cal.events.len(), 1);
        assert_eq!(cal.events[0].uid, "ev1");
        let reported: Vec<&str> = cal
            .notices
            .iter()
            .filter(|n| n.code == NoticeCode::UnsupportedComponent)
            .map(|n| n.detail.as_str())
            .collect();
        assert_eq!(reported.len(), 2, "{reported:?}");
        assert!(reported.iter().any(|d| d.starts_with("VTODO")));
        assert!(reported.iter().any(|d| d.starts_with("VALARM")));
    }

    /// A VTIMEZONE's own DTSTART must not be mistaken for the event's.
    #[test]
    fn properties_inside_an_unread_component_are_not_harvested() {
        let cal = parse(&wrap(
            "BEGIN:VTIMEZONE\r\nTZID=X\r\nBEGIN:DAYLIGHT\r\nDTSTART:19700308T020000\r\n\
END:DAYLIGHT\r\nEND:VTIMEZONE\r\n\
BEGIN:VEVENT\r\nUID:ev1\r\nDTSTART:20260301T090000Z\r\nEND:VEVENT\r\n",
        ))
        .unwrap();
        assert_eq!(cal.events.len(), 1);
        assert!(matches!(cal.events[0].dtstart, Some(ICalTime::Utc(_))));
    }

    #[test]
    fn user_content_we_cannot_hold_is_reported() {
        let cal = one("ATTACH:https://example.com/agenda.pdf\r\nCATEGORIES:work\r\n");
        let unmapped: Vec<&str> = cal
            .notices
            .iter()
            .filter(|n| n.code == NoticeCode::UnmappedProperty)
            .map(|n| n.detail.as_str())
            .collect();
        assert_eq!(unmapped.len(), 2, "{unmapped:?}");
        assert!(unmapped.iter().any(|d| d.starts_with("ATTACH")));
        assert!(unmapped.iter().any(|d| d.starts_with("CATEGORIES")));
    }

    #[test]
    fn bookkeeping_properties_are_dropped_without_noise() {
        let cal = one("DTSTAMP:20260228T090000Z\r\nSEQUENCE:3\r\nTRANSP:OPAQUE\r\n");
        assert!(cal.notices.is_empty(), "{:?}", cal.notices);
    }

    #[test]
    fn an_event_with_no_uid_is_skipped_and_reported() {
        let cal = parse(&wrap("BEGIN:VEVENT\r\nSUMMARY:Anonymous\r\nEND:VEVENT\r\n")).unwrap();
        assert!(cal.events.is_empty());
        assert_eq!(cal.notices[0].code, NoticeCode::Skipped);
    }

    #[test]
    fn round_trip() {
        let cal = parse(SAMPLE).unwrap();
        let written = write(&cal.events);
        let again = parse(&written).unwrap();
        assert_eq!(cal.events, again.events);
    }

    /// The kinds have to survive the write too, or an export would flatten
    /// what the import took care to keep.
    #[test]
    fn every_time_kind_round_trips_through_write() {
        let events = vec![ICalEvent {
            dtstart: Some(ICalTime::Zoned {
                civil: civil::date(2026, 3, 1).at(9, 0, 0, 0),
                tz: "America/New_York".into(),
            }),
            dtend: Some(ICalTime::Date(civil::date(2026, 3, 2))),
            ..ICalEvent::new("ev1")
        }];
        let back = parse(&write(&events)).unwrap();
        assert_eq!(back.events, events);

        let floating = vec![ICalEvent {
            dtstart: Some(ICalTime::Floating(civil::date(2026, 3, 1).at(9, 0, 0, 0))),
            dtend: Some(ICalTime::Utc(Timestamp::UNIX_EPOCH)),
            ..ICalEvent::new("ev2")
        }];
        assert_eq!(parse(&write(&floating)).unwrap().events, floating);
    }

    #[test]
    fn handles_line_continuation() {
        let cal = one("SUMMARY:long\r\n  text continued\r\n");
        assert_eq!(
            cal.events[0].summary.as_deref(),
            Some("long text continued")
        );
    }

    #[test]
    fn long_lines_are_folded_and_unfold_back() {
        let long = "x".repeat(300);
        let events = vec![ICalEvent {
            summary: Some(long.clone()),
            ..ICalEvent::new("ev1")
        }];
        let doc = write(&events);
        assert!(
            doc.lines().all(|l| l.trim_end().len() <= FOLD_AT),
            "every written line must respect the 75-octet limit"
        );
        assert_eq!(
            parse(&doc).unwrap().events[0].summary.as_deref(),
            Some(&long[..])
        );
    }

    /// Folding counts octets, and must never cut a character in half.
    #[test]
    fn folding_never_splits_a_multibyte_character() {
        let long = "é".repeat(200);
        let events = vec![ICalEvent {
            summary: Some(long.clone()),
            ..ICalEvent::new("ev1")
        }];
        let doc = write(&events);
        assert!(doc.lines().all(|l| l.trim_end().len() <= FOLD_AT));
        assert_eq!(
            parse(&doc).unwrap().events[0].summary.as_deref(),
            Some(&long[..])
        );
    }

    #[test]
    fn escaping_round_trips_every_special_character() {
        let text = "Hello, world; a\\b\nsecond line";
        let events = vec![ICalEvent {
            summary: Some(text.to_string()),
            description: Some(text.to_string()),
            ..ICalEvent::new("ev1")
        }];
        let back = parse(&write(&events)).unwrap();
        assert_eq!(back.events[0].summary.as_deref(), Some(text));
        assert_eq!(back.events[0].description.as_deref(), Some(text));
    }

    #[test]
    fn unescaping_leaves_an_undefined_escape_alone() {
        assert_eq!(unescape("a\\qb"), "a\\qb");
        assert_eq!(unescape("trailing\\"), "trailing\\");
    }

    #[test]
    fn lf_only_input_parses() {
        let lf = SAMPLE.replace("\r\n", "\n");
        assert_eq!(parse(&lf).unwrap().events.len(), 1);
    }
}

//! iCalendar (RFC 5545) import/export.
//!
//! v1 covers a meaningful subset:
//! - One `VEVENT` per calendar block: SUMMARY, DTSTART, DTEND, UID,
//!   DESCRIPTION, RRULE.
//! - Line unfolding (RFC 5545 §3.1) on import.
//! - Robust to whitespace and CRLF/LF line endings.
//!
//! Out of scope for v1: VTIMEZONE, VTODO, VJOURNAL, VFREEBUSY, valarms,
//! recurrence-id overrides, X- properties, attachments.

use crate::IntegrationError;
use jiff::{civil, tz::TimeZone, Timestamp};

/// One iCalendar event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ICalEvent {
    /// `UID` — unique id; required.
    pub uid: String,
    /// `SUMMARY` — display title.
    pub summary: Option<String>,
    /// `DTSTART` parsed to UTC.
    pub dtstart: Option<Timestamp>,
    /// `DTEND` parsed to UTC.
    pub dtend: Option<Timestamp>,
    /// `DESCRIPTION` — long-form text.
    pub description: Option<String>,
    /// Raw `RRULE` line (parsed downstream by `sunrise-domain::rrule`).
    pub rrule: Option<String>,
}

/// Parse an iCalendar document. Returns one event per `VEVENT` block.
pub fn parse(input: &str) -> Result<Vec<ICalEvent>, IntegrationError> {
    let lines = unfold_lines(input);
    let mut events: Vec<ICalEvent> = Vec::new();
    let mut current: Option<ICalEvent> = None;
    for raw in lines {
        let line = raw.trim_end();
        if line.is_empty() {
            continue;
        }
        if line.eq_ignore_ascii_case("BEGIN:VCALENDAR") {
            continue;
        }
        if line.eq_ignore_ascii_case("END:VCALENDAR") {
            continue;
        }
        if line.eq_ignore_ascii_case("BEGIN:VEVENT") {
            current = Some(ICalEvent {
                uid: String::new(),
                summary: None,
                dtstart: None,
                dtend: None,
                description: None,
                rrule: None,
            });
            continue;
        }
        if line.eq_ignore_ascii_case("END:VEVENT") {
            if let Some(ev) = current.take() {
                if !ev.uid.is_empty() {
                    events.push(ev);
                }
            }
            continue;
        }
        let Some(ev) = current.as_mut() else {
            continue;
        };
        let (name, value) = split_property(line);
        match name.to_ascii_uppercase().as_str() {
            "UID" => ev.uid = value.to_string(),
            "SUMMARY" => ev.summary = Some(unescape(value)),
            "DESCRIPTION" => ev.description = Some(unescape(value)),
            "DTSTART" => ev.dtstart = parse_datetime(value),
            "DTEND" => ev.dtend = parse_datetime(value),
            "RRULE" => ev.rrule = Some(value.to_string()),
            _ => {}
        }
    }
    Ok(events)
}

/// Render a list of events as an iCalendar document.
pub fn write(events: &[ICalEvent]) -> String {
    let mut out = String::with_capacity(events.len() * 256);
    out.push_str("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Sunrise//Sunrise v1//EN\r\n");
    for e in events {
        out.push_str("BEGIN:VEVENT\r\n");
        out.push_str(&format!("UID:{}\r\n", e.uid));
        if let Some(s) = &e.summary {
            out.push_str(&format!("SUMMARY:{}\r\n", escape(s)));
        }
        if let Some(d) = e.dtstart {
            out.push_str(&format!("DTSTART:{}\r\n", format_dt(d)));
        }
        if let Some(d) = e.dtend {
            out.push_str(&format!("DTEND:{}\r\n", format_dt(d)));
        }
        if let Some(s) = &e.description {
            out.push_str(&format!("DESCRIPTION:{}\r\n", escape(s)));
        }
        if let Some(r) = &e.rrule {
            out.push_str(&format!("RRULE:{r}\r\n"));
        }
        out.push_str("END:VEVENT\r\n");
    }
    out.push_str("END:VCALENDAR\r\n");
    out
}

fn unfold_lines(input: &str) -> Vec<String> {
    // RFC 5545 §3.1: a line that begins with a SPACE or HTAB is a
    // continuation of the previous logical line.
    let mut out: Vec<String> = Vec::new();
    for raw in input.lines() {
        if let Some(prev) = out.last_mut() {
            if raw.starts_with(' ') || raw.starts_with('\t') {
                prev.push_str(&raw[1..]);
                continue;
            }
        }
        out.push(raw.to_string());
    }
    out
}

fn split_property(line: &str) -> (&str, &str) {
    // Property name + parameters end at the first ':'. Parameters use ';'.
    let split = line.find(':').unwrap_or(line.len());
    let head = &line[..split];
    let value = if split == line.len() {
        ""
    } else {
        &line[split + 1..]
    };
    let name_end = head.find(';').unwrap_or(head.len());
    let name = &head[..name_end];
    (name, value)
}

fn parse_datetime(s: &str) -> Option<Timestamp> {
    // YYYYMMDD'T'HHMMSS('Z')? — parse as civil (wall-clock) then pin to UTC.
    let s = s.trim_end_matches('Z');
    let dt: civil::DateTime = civil::DateTime::strptime("%Y%m%dT%H%M%S", s).ok()?;
    dt.to_zoned(TimeZone::UTC).ok().map(|z| z.timestamp())
}

fn format_dt(dt: Timestamp) -> String {
    dt.strftime("%Y%m%dT%H%M%SZ").to_string()
}

fn unescape(s: &str) -> String {
    s.replace("\\n", "\n")
        .replace("\\,", ",")
        .replace("\\;", ";")
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace(',', "\\,")
        .replace(';', "\\;")
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

    #[test]
    fn parse_one_vevent() {
        let evs = parse(SAMPLE).unwrap();
        assert_eq!(evs.len(), 1);
        let e = &evs[0];
        assert_eq!(e.uid, "ev1@example.com");
        assert_eq!(e.summary.as_deref(), Some("Coffee with Alice"));
        assert!(e.dtstart.is_some());
        assert!(e.dtend.is_some());
    }

    #[test]
    fn parse_skips_unknown_props() {
        let s = format!("{}", SAMPLE).replace(
            "DESCRIPTION:Discuss the launch plan\r\n",
            "X-CUSTOM:hello\r\nDESCRIPTION:Discuss the launch plan\r\n",
        );
        let evs = parse(&s).unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(
            evs[0].description.as_deref(),
            Some("Discuss the launch plan")
        );
    }

    #[test]
    fn round_trip() {
        let evs = parse(SAMPLE).unwrap();
        let written = write(&evs);
        let evs2 = parse(&written).unwrap();
        assert_eq!(evs, evs2);
    }

    #[test]
    fn handles_line_continuation() {
        let s = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:ev1\r\nSUMMARY:long\r\n  text continued\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let evs = parse(s).unwrap();
        assert_eq!(evs[0].summary.as_deref(), Some("long text continued"));
    }

    #[test]
    fn unescaping() {
        let s = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:ev1\r\nSUMMARY:Hello\\, world\\;\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let evs = parse(s).unwrap();
        assert_eq!(evs[0].summary.as_deref(), Some("Hello, world;"));
    }
}

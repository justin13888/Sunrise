//! RFC 5545 RRULE parser (supported subset) per `docs/02-domain/routines-and-recurrence.md`.
//!
//! Supported today: `FREQ`, `INTERVAL`, `BYDAY`, `BYMONTHDAY`, `BYMONTH`,
//! `BYSETPOS`, `COUNT`, `UNTIL`, `WKST`. NOT supported: `BYYEARDAY`,
//! `BYWEEKNO`. Extensions (floating windows, adaptive cadence) are out of
//! scope here.
//!
//! This module implements parsing + recognition; full DST-aware expansion
//! lives in [`crate::routine_gen`].

use crate::unknown::UnknownVariant;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Recurrence frequency.
///
/// An unrecognised frequency is kept verbatim (ADR-0045 §6) and has no safe
/// reading: a rule that holds one generates no occurrences (see
/// [`RRule::is_understood`]), because recurring on a guessed cadence is worse
/// than not recurring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frequency {
    /// Once a day, plus `INTERVAL`.
    Daily,
    /// Once a week.
    Weekly,
    /// Once a month.
    Monthly,
    /// Once a year.
    Yearly,
    /// A frequency this build does not know, kept verbatim.
    Unknown(UnknownVariant),
}

crate::unknown::lossy_enum!(Frequency, {
    Daily => "DAILY",
    Weekly => "WEEKLY",
    Monthly => "MONTHLY",
    Yearly => "YEARLY",
});

impl Frequency {
    /// Parse a known RFC 5545 spelling (uppercase); `None` for anything else.
    /// [`Frequency::from_raw`] is the lossless form.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(Self::from_raw(s)).filter(|f| !f.is_unknown())
    }
}

/// Day-of-week token used in `BYDAY`.
///
/// An unrecognised token is kept verbatim (ADR-0045 §6) and matches no day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Weekday {
    /// Sunday.
    Su,
    /// Monday.
    Mo,
    /// Tuesday.
    Tu,
    /// Wednesday.
    We,
    /// Thursday.
    Th,
    /// Friday.
    Fr,
    /// Saturday.
    Sa,
    /// A day token this build does not know, kept verbatim.
    Unknown(UnknownVariant),
}

crate::unknown::lossy_enum!(Weekday, {
    Su => "SU",
    Mo => "MO",
    Tu => "TU",
    We => "WE",
    Th => "TH",
    Fr => "FR",
    Sa => "SA",
});

impl Weekday {
    /// Parse a known 2-letter token; `None` for anything else.
    /// [`Weekday::from_raw`] is the lossless form.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(Self::from_raw(s)).filter(|w| !w.is_unknown())
    }
}

/// Parsed RRULE.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RRule {
    /// `FREQ` (required).
    pub freq: Frequency,
    /// `INTERVAL` (defaults to 1).
    pub interval: u32,
    /// `BYDAY` (optional).
    #[serde(default)]
    pub by_day: Vec<Weekday>,
    /// `BYMONTHDAY` (optional). Negative values are 1-based-from-end.
    #[serde(default)]
    pub by_month_day: Vec<i32>,
    /// `BYMONTH` 1..=12 (optional).
    #[serde(default)]
    pub by_month: Vec<u32>,
    /// `BYSETPOS` (optional).
    #[serde(default)]
    pub by_set_pos: Vec<i32>,
    /// `COUNT` (optional).
    #[serde(default)]
    pub count: Option<u32>,
    /// `UNTIL` (optional, UTC).
    #[serde(default)]
    pub until: Option<Timestamp>,
    /// `WKST` (optional; defaults to Monday per RFC 5545).
    #[serde(default)]
    pub wkst: Option<Weekday>,
}

/// RRULE parse errors.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RRuleParseError {
    /// Required `FREQ` token missing.
    #[error("RRULE missing FREQ")]
    MissingFreq,
    /// Unknown / unsupported part name.
    #[error("RRULE unknown part: {0}")]
    UnknownPart(String),
    /// Value couldn't be parsed.
    #[error("RRULE bad value for {0}: {1}")]
    BadValue(&'static str, String),
}

impl RRule {
    /// Parse an RFC 5545-style RRULE body (no leading `RRULE:` prefix; the
    /// caller strips that). Tokens are `;`-separated `KEY=VALUE` pairs.
    pub fn parse(body: &str) -> Result<Self, RRuleParseError> {
        let mut freq: Option<Frequency> = None;
        let mut out = Self {
            freq: Frequency::Daily,
            interval: 1,
            by_day: Vec::new(),
            by_month_day: Vec::new(),
            by_month: Vec::new(),
            by_set_pos: Vec::new(),
            count: None,
            until: None,
            wkst: None,
        };
        for part in body.split(';').filter(|p| !p.is_empty()) {
            let (k, v) = part
                .split_once('=')
                .ok_or_else(|| RRuleParseError::UnknownPart(part.to_string()))?;
            match k {
                // A value this build does not know is kept, not refused
                // (ADR-0045 §6): this parser also reads the stored rule back.
                // Input a user typed is refused one level up, in
                // `crate::recur::parse_recurrence`, via `first_unknown`.
                "FREQ" => {
                    freq = Some(Frequency::from_raw(v));
                }
                "INTERVAL" => {
                    out.interval = v
                        .parse()
                        .map_err(|_| RRuleParseError::BadValue("INTERVAL", v.into()))?;
                    if out.interval == 0 {
                        return Err(RRuleParseError::BadValue("INTERVAL", v.into()));
                    }
                }
                "BYDAY" => {
                    for tok in v.split(',') {
                        out.by_day.push(Weekday::from_raw(tok));
                    }
                }
                "BYMONTHDAY" => {
                    for tok in v.split(',') {
                        out.by_month_day
                            .push(tok.parse().map_err(|_| {
                                RRuleParseError::BadValue("BYMONTHDAY", tok.into())
                            })?);
                    }
                }
                "BYMONTH" => {
                    for tok in v.split(',') {
                        out.by_month.push(
                            tok.parse()
                                .map_err(|_| RRuleParseError::BadValue("BYMONTH", tok.into()))?,
                        );
                    }
                }
                "BYSETPOS" => {
                    for tok in v.split(',') {
                        out.by_set_pos.push(
                            tok.parse()
                                .map_err(|_| RRuleParseError::BadValue("BYSETPOS", tok.into()))?,
                        );
                    }
                }
                "COUNT" => {
                    out.count = Some(
                        v.parse()
                            .map_err(|_| RRuleParseError::BadValue("COUNT", v.into()))?,
                    );
                }
                "UNTIL" => {
                    let parsed: Timestamp = v
                        .parse()
                        .map_err(|_| RRuleParseError::BadValue("UNTIL", v.into()))?;
                    out.until = Some(parsed);
                }
                "WKST" => {
                    out.wkst = Some(Weekday::from_raw(v));
                }
                "BYYEARDAY" | "BYWEEKNO" => {
                    return Err(RRuleParseError::UnknownPart(format!(
                        "{k} not in the supported subset"
                    )));
                }
                other => return Err(RRuleParseError::UnknownPart(other.into())),
            }
        }
        out.freq = freq.ok_or(RRuleParseError::MissingFreq)?;
        Ok(out)
    }

    /// Render back to a canonical RFC 5545 RRULE body (no `RRULE:` prefix).
    ///
    /// Round-trips losslessly through [`RRule::parse`]: only non-default parts
    /// are emitted (`INTERVAL=1` and an unset `WKST` are omitted). Used by the
    /// storage layer to persist the rule as text.
    ///
    /// That holds for every rule [`RRule::parse`] produces. It does not hold
    /// for a rule that arrived as CBOR holding an unknown value with `;`, `,`
    /// or `=` in it, which is why storage also keeps the rule's canonical CBOR
    /// (`routines.rrule_cbor`) and reads that first.
    #[must_use]
    pub fn to_rfc5545(&self) -> String {
        let mut parts = vec![format!("FREQ={}", self.freq.as_str())];
        if self.interval != 1 {
            parts.push(format!("INTERVAL={}", self.interval));
        }
        if !self.by_day.is_empty() {
            let days: Vec<&str> = self.by_day.iter().map(Weekday::as_str).collect();
            parts.push(format!("BYDAY={}", days.join(",")));
        }
        if !self.by_month_day.is_empty() {
            let v: Vec<String> = self.by_month_day.iter().map(ToString::to_string).collect();
            parts.push(format!("BYMONTHDAY={}", v.join(",")));
        }
        if !self.by_month.is_empty() {
            let v: Vec<String> = self.by_month.iter().map(ToString::to_string).collect();
            parts.push(format!("BYMONTH={}", v.join(",")));
        }
        if !self.by_set_pos.is_empty() {
            let v: Vec<String> = self.by_set_pos.iter().map(ToString::to_string).collect();
            parts.push(format!("BYSETPOS={}", v.join(",")));
        }
        if let Some(c) = self.count {
            parts.push(format!("COUNT={c}"));
        }
        if let Some(u) = self.until {
            parts.push(format!("UNTIL={u}"));
        }
        if let Some(w) = &self.wkst {
            parts.push(format!("WKST={}", w.as_str()));
        }
        parts.join(";")
    }

    /// The first value in the rule this build does not know, as
    /// `(part, raw value)`; `None` when every value is known.
    #[must_use]
    pub fn first_unknown(&self) -> Option<(&'static str, &str)> {
        if let Frequency::Unknown(raw) = &self.freq {
            return Some(("FREQ", raw.as_str()));
        }
        if let Some(Weekday::Unknown(raw)) = self.by_day.iter().find(|w| w.is_unknown()) {
            return Some(("BYDAY", raw.as_str()));
        }
        if let Some(Weekday::Unknown(raw)) = &self.wkst {
            return Some(("WKST", raw.as_str()));
        }
        None
    }

    /// Whether this build can expand the rule: every `FREQ`, `BYDAY` and
    /// `WKST` value is one it knows.
    ///
    /// A rule that is not understood generates **no occurrences** and is
    /// flagged in its summary (ADR-0045 §6). It is still kept and written back
    /// verbatim; only its expansion stops. Recurring on a guessed schedule
    /// would materialize tasks on the wrong days, which is worse than none.
    #[must_use]
    pub fn is_understood(&self) -> bool {
        self.first_unknown().is_none()
    }
}

/// Human-readable one-line summary of an [`RRule`] — the inverse of
/// [`crate::recur::parse_recurrence`].
///
/// Lossy on purpose: it is prose for a list row, not a serialization. The
/// round-trippable form is [`RRule::to_rfc5545`].
#[must_use]
pub fn rrule_summary(r: &RRule) -> String {
    use std::fmt::Write as _;
    // The flag ADR-0045 §6 asks for: a rule this build cannot expand says so,
    // rather than reading as a schedule it is not following.
    if !r.is_understood() {
        return format!("unrecognised schedule ({})", r.to_rfc5545());
    }
    let unit = match r.freq {
        Frequency::Daily => "day",
        Frequency::Weekly => "week",
        Frequency::Monthly => "month",
        Frequency::Yearly => "year",
        // Unreachable behind `is_understood`, and harmless if not.
        Frequency::Unknown(_) => "period",
    };
    let mut s = if r.interval <= 1 {
        format!("every {unit}")
    } else {
        format!("every {} {unit}s", r.interval)
    };
    if !r.by_day.is_empty() {
        let days: Vec<String> = r.by_day.iter().map(|d| format!("{d:?}")).collect();
        s.push_str(" on ");
        s.push_str(&days.join(", "));
    }
    if !r.by_month_day.is_empty() {
        let days: Vec<String> = r.by_month_day.iter().map(ToString::to_string).collect();
        s.push_str(" day ");
        s.push_str(&days.join(", "));
    }
    if let Some(c) = r.count {
        let _ = write!(s, " \u{00d7}{c}");
    }
    if let Some(u) = r.until {
        let _ = write!(s, " until {u}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_daily() {
        let r = RRule::parse("FREQ=DAILY").unwrap();
        assert_eq!(r.freq, Frequency::Daily);
        assert_eq!(r.interval, 1);
        assert!(r.by_day.is_empty());
    }

    #[test]
    fn parse_weekly_byday() {
        let r = RRule::parse("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE,FR").unwrap();
        assert_eq!(r.freq, Frequency::Weekly);
        assert_eq!(r.interval, 2);
        assert_eq!(r.by_day, vec![Weekday::Mo, Weekday::We, Weekday::Fr]);
    }

    #[test]
    fn parse_monthly_bymonthday_negative() {
        let r = RRule::parse("FREQ=MONTHLY;BYMONTHDAY=-1").unwrap();
        assert_eq!(r.by_month_day, vec![-1]);
    }

    #[test]
    fn rejects_missing_freq() {
        assert_eq!(
            RRule::parse("INTERVAL=2"),
            Err(RRuleParseError::MissingFreq)
        );
    }

    #[test]
    fn rejects_unsupported_byyearday() {
        let err = RRule::parse("FREQ=YEARLY;BYYEARDAY=100").unwrap_err();
        assert!(matches!(err, RRuleParseError::UnknownPart(_)));
    }

    #[test]
    fn rejects_zero_interval() {
        assert_eq!(
            RRule::parse("FREQ=DAILY;INTERVAL=0"),
            Err(RRuleParseError::BadValue("INTERVAL", "0".into()))
        );
    }

    #[test]
    fn parse_until_iso() {
        let r = RRule::parse("FREQ=DAILY;UNTIL=2027-01-01T00:00:00Z").unwrap();
        // The instant itself, not just its presence: `UNTIL` is the bound that
        // stops a routine generating tasks forever, and `is_some()` would hold
        // for any instant at all.
        assert_eq!(
            r.until,
            Some("2027-01-01T00:00:00Z".parse::<Timestamp>().unwrap())
        );
        assert_eq!(
            r.until.unwrap().as_millisecond(),
            1_798_761_600_000,
            "2027-01-01T00:00:00Z in epoch milliseconds"
        );
    }

    #[test]
    fn unknown_values_are_kept_and_flagged_not_refused() {
        let r = RRule::parse("FREQ=HOURLY;BYDAY=MO,XX;WKST=ZZ").unwrap();
        assert_eq!(r.freq.as_str(), "HOURLY");
        assert_eq!(r.by_day[0], Weekday::Mo);
        assert_eq!(r.by_day[1].as_str(), "XX");
        assert_eq!(r.wkst.as_ref().map(Weekday::as_str), Some("ZZ"));
        assert_eq!(r.to_rfc5545(), "FREQ=HOURLY;BYDAY=MO,XX;WKST=ZZ");
        assert_eq!(r.first_unknown(), Some(("FREQ", "HOURLY")));
        assert!(!r.is_understood());
        assert!(rrule_summary(&r).starts_with("unrecognised schedule"));

        let day_only = RRule::parse("FREQ=WEEKLY;BYDAY=XX").unwrap();
        assert_eq!(day_only.first_unknown(), Some(("BYDAY", "XX")));
        let wkst_only = RRule::parse("FREQ=WEEKLY;WKST=ZZ").unwrap();
        assert_eq!(wkst_only.first_unknown(), Some(("WKST", "ZZ")));
        assert!(RRule::parse("FREQ=WEEKLY;BYDAY=MO")
            .unwrap()
            .is_understood());
    }

    #[test]
    fn to_rfc5545_round_trips() {
        for body in [
            "FREQ=DAILY",
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE,FR;WKST=SU",
            "FREQ=MONTHLY;BYDAY=FR;BYSETPOS=-1",
            "FREQ=MONTHLY;BYMONTHDAY=-1,15;BYMONTH=1,6,12",
            "FREQ=DAILY;COUNT=10",
            "FREQ=YEARLY;UNTIL=2027-01-01T00:00:00Z",
        ] {
            let parsed = RRule::parse(body).unwrap();
            let reparsed = RRule::parse(&parsed.to_rfc5545()).unwrap();
            assert_eq!(parsed, reparsed, "round-trip mismatch for {body}");
        }
    }
}

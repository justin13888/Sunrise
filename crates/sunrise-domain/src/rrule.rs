//! RFC 5545 RRULE parser (v1 subset) per `docs/02-domain/routines-and-recurrence.md`.
//!
//! Supported in v1: `FREQ`, `INTERVAL`, `BYDAY`, `BYMONTHDAY`, `BYMONTH`,
//! `BYSETPOS`, `COUNT`, `UNTIL`, `WKST`. NOT supported: `BYYEARDAY`,
//! `BYWEEKNO`. Extensions (floating windows, adaptive cadence) are out of
//! scope here.
//!
//! v1 implements parsing + recognition; full DST-aware expansion lives in
//! `crates/sunrise-domain::routine_gen` (TODO Phase 5/6 timing).

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Recurrence frequency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Frequency {
    /// Once a day, plus `INTERVAL`.
    Daily,
    /// Once a week.
    Weekly,
    /// Once a month.
    Monthly,
    /// Once a year.
    Yearly,
}

impl Frequency {
    /// Parse from canonical RFC 5545 spelling (uppercase).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "DAILY" => Some(Self::Daily),
            "WEEKLY" => Some(Self::Weekly),
            "MONTHLY" => Some(Self::Monthly),
            "YEARLY" => Some(Self::Yearly),
            _ => None,
        }
    }
}

/// Day-of-week token used in `BYDAY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
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
}

impl Weekday {
    /// Parse from canonical 2-letter token.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "SU" => Some(Self::Su),
            "MO" => Some(Self::Mo),
            "TU" => Some(Self::Tu),
            "WE" => Some(Self::We),
            "TH" => Some(Self::Th),
            "FR" => Some(Self::Fr),
            "SA" => Some(Self::Sa),
            _ => None,
        }
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
                "FREQ" => {
                    freq = Some(
                        Frequency::parse(v)
                            .ok_or_else(|| RRuleParseError::BadValue("FREQ", v.into()))?,
                    );
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
                        out.by_day.push(
                            Weekday::parse(tok)
                                .ok_or_else(|| RRuleParseError::BadValue("BYDAY", tok.into()))?,
                        );
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
                    out.wkst = Some(
                        Weekday::parse(v)
                            .ok_or_else(|| RRuleParseError::BadValue("WKST", v.into()))?,
                    );
                }
                "BYYEARDAY" | "BYWEEKNO" => {
                    return Err(RRuleParseError::UnknownPart(format!(
                        "{k} not in v1 subset"
                    )));
                }
                other => return Err(RRuleParseError::UnknownPart(other.into())),
            }
        }
        out.freq = freq.ok_or(RRuleParseError::MissingFreq)?;
        Ok(out)
    }
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
        assert!(r.until.is_some());
    }
}

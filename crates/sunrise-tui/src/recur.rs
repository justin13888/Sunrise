//! Plain-English recurrence, parsed into an [`RRule`].
//!
//! Routines were read-only in the TUI. The empty state said to create them
//! "from the desktop client" — a client that no longer exists, so the entire
//! recurrence engine (RRULE expansion, DST-correct materialisation, streaks,
//! catch-up policy) had no way in from anything a user could run.
//!
//! The obstacle is the input. `RRULE:FREQ=WEEKLY;BYDAY=MO,WE` is a wire
//! format, not something to type at a prompt, and the domain's parser only
//! accepts that form. This module is the front end: it reads the phrases
//! people actually use, and falls through to the RFC 5545 body for anyone who
//! wants to write one exactly.
//!
//! Recognised forms:
//!
//! | Input | Meaning |
//! |---|---|
//! | `daily`, `every day` | `FREQ=DAILY` |
//! | `every 3 days` | `FREQ=DAILY;INTERVAL=3` |
//! | `weekly`, `every week` | `FREQ=WEEKLY` |
//! | `every monday`, `every mon, thu` | `FREQ=WEEKLY;BYDAY=…` |
//! | `weekdays` | `FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR` |
//! | `weekends` | `FREQ=WEEKLY;BYDAY=SA,SU` |
//! | `every 2 weeks on tue` | interval + `BYDAY` |
//! | `monthly`, `every month` | `FREQ=MONTHLY` |
//! | `monthly on day 1`, `on the 15th` | `BYMONTHDAY` |
//! | `monthly on the last day` | `BYMONTHDAY=-1` |
//! | `yearly`, `annually` | `FREQ=YEARLY` |
//! | `… x12` | `COUNT=12` |
//! | `FREQ=WEEKLY;BYDAY=MO` | passed straight through |
//!
//! Nothing here guesses. An input it does not recognise is an error naming
//! what it saw, because a routine that silently fires on the wrong cadence is
//! discovered weeks later, by which point it has generated the wrong tasks.

use sunrise_domain::rrule::{Frequency, RRule, Weekday};

/// Parse a recurrence phrase (or a raw RFC 5545 body) into an [`RRule`].
///
/// # Errors
///
/// Returns a human-readable message naming the token that could not be read.
pub fn parse_recurrence(input: &str) -> Result<RRule, String> {
    let text = input.trim();
    if text.is_empty() {
        return Err("recurrence is required (try \"every day\")".into());
    }
    // An RFC 5545 body is unmistakable and the domain already parses it, so
    // hand it over rather than reimplementing the grammar loosely.
    if text.to_ascii_uppercase().contains("FREQ=") {
        let body = text
            .trim_start_matches("RRULE:")
            .trim_start_matches("rrule:");
        return RRule::parse(body).map_err(|e| e.to_string());
    }

    let lower = text.to_ascii_lowercase();
    let mut words: Vec<&str> = lower
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|w| !w.is_empty())
        .collect();

    let mut rule = blank();
    let count = take_count(&mut words)?;
    rule.count = count;

    // Drop a leading "every"; "every day" and "daily" are the same request.
    if words.first() == Some(&"every") {
        words.remove(0);
    }
    if words.is_empty() {
        return Err(format!("cannot read the recurrence \"{text}\""));
    }

    // A leading number is the interval: "every 3 days", "every 2 weeks on tue".
    let mut i = 0usize;
    if let Ok(n) = words[0].parse::<u32>() {
        if n == 0 {
            return Err("interval must be at least 1".into());
        }
        rule.interval = n;
        i = 1;
    }
    if i >= words.len() {
        return Err(format!("cannot read the recurrence \"{text}\""));
    }

    match words[i] {
        "day" | "days" | "daily" => rule.freq = Frequency::Daily,
        "week" | "weeks" | "weekly" => rule.freq = Frequency::Weekly,
        "month" | "months" | "monthly" => rule.freq = Frequency::Monthly,
        "year" | "years" | "yearly" | "annually" => rule.freq = Frequency::Yearly,
        "weekday" | "weekdays" => {
            rule.freq = Frequency::Weekly;
            rule.by_day = vec![
                Weekday::Mo,
                Weekday::Tu,
                Weekday::We,
                Weekday::Th,
                Weekday::Fr,
            ];
        }
        "weekend" | "weekends" => {
            rule.freq = Frequency::Weekly;
            rule.by_day = vec![Weekday::Sa, Weekday::Su];
        }
        other => {
            // "every monday" / "every mon, thu": a bare day list means weekly.
            let days = read_days(&words[i..])?;
            if days.is_empty() {
                return Err(format!("cannot read \"{other}\" as a recurrence"));
            }
            rule.freq = Frequency::Weekly;
            rule.by_day = days;
            return Ok(rule);
        }
    }
    i += 1;

    // Optional tail: "on mon, wed", "on day 1", "on the 15th", "on the last day".
    if i < words.len() {
        if words[i] != "on" {
            return Err(format!("unexpected \"{}\" in the recurrence", words[i]));
        }
        i += 1;
        let tail: Vec<&str> = words[i..].iter().copied().filter(|w| *w != "the").collect();
        if tail.is_empty() {
            return Err("\"on\" needs a day (try \"on monday\" or \"on day 1\")".into());
        }
        match rule.freq {
            Frequency::Weekly => rule.by_day = read_days(&tail)?,
            Frequency::Monthly | Frequency::Yearly => rule.by_month_day = read_month_days(&tail)?,
            Frequency::Daily => {
                return Err("a daily rule has no \"on\" clause".into());
            }
        }
    }
    Ok(rule)
}

/// Split a trailing `x12` occurrence count off the word list.
fn take_count(words: &mut Vec<&str>) -> Result<Option<u32>, String> {
    let Some(last) = words.last().copied() else {
        return Ok(None);
    };
    let Some(digits) = last.strip_prefix('x') else {
        return Ok(None);
    };
    let n: u32 = digits
        .parse()
        .map_err(|_| format!("\"{last}\" is not an occurrence count (try x12)"))?;
    if n == 0 {
        return Err("an occurrence count must be at least 1".into());
    }
    words.pop();
    Ok(Some(n))
}

/// Read a list of weekday names or abbreviations.
fn read_days(words: &[&str]) -> Result<Vec<Weekday>, String> {
    let mut out = Vec::new();
    for w in words {
        let d = weekday(w).ok_or_else(|| format!("\"{w}\" is not a weekday"))?;
        if !out.contains(&d) {
            out.push(d);
        }
    }
    Ok(out)
}

/// Read a list of month-days: `1`, `15th`, `last`.
fn read_month_days(words: &[&str]) -> Result<Vec<i32>, String> {
    let mut out = Vec::new();
    for w in words {
        if *w == "day" || *w == "days" {
            continue;
        }
        if *w == "last" {
            out.push(-1);
            continue;
        }
        // "15th" / "1st" / "22nd" all end in two letters that carry no
        // information here.
        let digits = w.trim_end_matches(|c: char| c.is_ascii_alphabetic());
        let n: i32 = digits
            .parse()
            .map_err(|_| format!("\"{w}\" is not a day of the month"))?;
        if !(1..=31).contains(&n) {
            return Err(format!("day {n} is not in 1-31"));
        }
        out.push(n);
    }
    if out.is_empty() {
        return Err("\"on\" needs a day of the month (try \"on day 1\")".into());
    }
    Ok(out)
}

/// Full names and the usual abbreviations.
fn weekday(w: &str) -> Option<Weekday> {
    Some(match w.trim_end_matches('s') {
        "mo" | "mon" | "monday" => Weekday::Mo,
        "tu" | "tue" | "tues" | "tuesday" => Weekday::Tu,
        "we" | "wed" | "wednesday" => Weekday::We,
        "th" | "thu" | "thur" | "thurs" | "thursday" => Weekday::Th,
        "fr" | "fri" | "friday" => Weekday::Fr,
        "sa" | "sat" | "saturday" => Weekday::Sa,
        "su" | "sun" | "sunday" => Weekday::Su,
        _ => return None,
    })
}

/// A daily rule with everything else unset — the base every branch edits.
fn blank() -> RRule {
    RRule {
        freq: Frequency::Daily,
        interval: 1,
        by_day: Vec::new(),
        by_month_day: Vec::new(),
        by_month: Vec::new(),
        by_set_pos: Vec::new(),
        count: None,
        until: None,
        wkst: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(input: &str) -> RRule {
        parse_recurrence(input).unwrap_or_else(|e| panic!("{input:?}: {e}"))
    }

    #[test]
    fn the_everyday_phrases_all_land_on_daily() {
        for text in ["daily", "every day", "Every Day", "  every   day  "] {
            let r = ok(text);
            assert_eq!(r.freq, Frequency::Daily, "{text}");
            assert_eq!(r.interval, 1, "{text}");
        }
    }

    #[test]
    fn an_interval_is_read_from_the_number() {
        let r = ok("every 3 days");
        assert_eq!(r.freq, Frequency::Daily);
        assert_eq!(r.interval, 3);
        let r = ok("every 2 weeks on tue");
        assert_eq!(r.freq, Frequency::Weekly);
        assert_eq!(r.interval, 2);
        assert_eq!(r.by_day, vec![Weekday::Tu]);
    }

    #[test]
    fn a_bare_day_list_means_weekly() {
        let r = ok("every mon, thu");
        assert_eq!(r.freq, Frequency::Weekly);
        assert_eq!(r.by_day, vec![Weekday::Mo, Weekday::Th]);
        assert_eq!(ok("every monday").by_day, vec![Weekday::Mo]);
    }

    #[test]
    fn weekdays_and_weekends_are_their_own_words() {
        assert_eq!(ok("weekdays").by_day.len(), 5);
        assert_eq!(ok("weekends").by_day, vec![Weekday::Sa, Weekday::Su]);
    }

    #[test]
    fn monthly_reads_a_day_of_the_month() {
        assert_eq!(ok("monthly on day 1").by_month_day, vec![1]);
        assert_eq!(ok("monthly on the 15th").by_month_day, vec![15]);
        // Negative is 1-based-from-end in RFC 5545, which is how "last day"
        // survives February.
        assert_eq!(ok("monthly on the last day").by_month_day, vec![-1]);
    }

    #[test]
    fn a_trailing_count_ends_the_series() {
        let r = ok("every week x12");
        assert_eq!(r.freq, Frequency::Weekly);
        assert_eq!(r.count, Some(12));
    }

    #[test]
    fn an_rfc_5545_body_is_handed_to_the_domain_parser() {
        let r = ok("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE");
        assert_eq!(r.freq, Frequency::Weekly);
        assert_eq!(r.interval, 2);
        assert_eq!(r.by_day, vec![Weekday::Mo, Weekday::We]);
        // …including with the wire prefix people copy along with it.
        assert_eq!(ok("RRULE:FREQ=DAILY").freq, Frequency::Daily);
    }

    #[test]
    fn nothing_is_guessed() {
        // A routine that silently fires on the wrong cadence is discovered
        // weeks later, after it has generated the wrong tasks.
        for bad in [
            "",
            "sometimes",
            "every blursday",
            "every 0 days",
            "every day on monday",
            "monthly on day 99",
            "every week x0",
            "every week backwards",
        ] {
            assert!(parse_recurrence(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn an_error_names_what_it_could_not_read() {
        let e = parse_recurrence("every blursday").unwrap_err();
        assert!(e.contains("blursday"), "{e}");
    }

    #[test]
    fn a_parsed_rule_round_trips_through_the_wire_form() {
        // The proof that this front end produces rules the engine accepts:
        // everything it emits must survive the domain's own parser.
        for text in [
            "every day",
            "every 3 days",
            "weekdays",
            "every 2 weeks on tue, thu",
            "monthly on the last day",
            "yearly",
            "every week x12",
        ] {
            let rule = ok(text);
            let body = rule.to_rfc5545();
            let body = body.trim_start_matches("RRULE:");
            assert_eq!(RRule::parse(body).unwrap(), rule, "{text} → {body}");
        }
    }
}

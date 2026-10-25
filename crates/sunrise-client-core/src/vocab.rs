//! The shared vocabulary's client-side derivations: the reader's zone, the
//! epoch-millisecond edges a client speaks in, and the few answers that are
//! more than one domain call.
//!
//! Each rule here is a [`sunrise_domain::phrase`] or [`sunrise_domain::planning`]
//! answer read in **the zone the caller names**, which every client passes as
//! an IANA string and nothing here reads from the host. "Overdue",
//! "tomorrow" and "defer until next week" are each two lines to write in a
//! client, and each of those two-line versions would disagree with the next
//! client the first time a deadline landed on a DST boundary. The `UniFFI`
//! seam lowers its records onto these, and every other client calls them
//! directly.
//!
//! An unknown zone name falls back to UTC everywhere in this module, for the
//! reason the capture path gives: showing a row in the wrong section is a
//! smaller failure than showing no rows at all.

use jiff::tz::TimeZone;
use jiff::Timestamp;
use sunrise_domain::{
    Block, BlockDraft, BlockOverlap, FocusSession, Routine, SnoozeSpan, SunriseTime, TodaySection,
    ValidationError,
};

/// An IANA zone, or UTC when the name is not one.
#[must_use]
pub fn zone_or_utc(tz: &str) -> TimeZone {
    TimeZone::get(tz).unwrap_or(TimeZone::UTC)
}

/// `ms` as an instant. A value `jiff` cannot represent — past year 9999, or
/// past `i64::MAX` — reads as the epoch rather than wrapping negative.
#[must_use]
pub fn instant_at(ms: u64) -> Timestamp {
    Timestamp::from_millisecond(i64::try_from(ms).unwrap_or(i64::MAX))
        .unwrap_or(Timestamp::UNIX_EPOCH)
}

/// Which section of Today a row belongs in, read in `tz`.
///
/// See [`sunrise_domain::today_section`]: the overdue boundary is
/// `due_at < start_of_today_local`, which is **not** `due_at < now_ms`.
#[must_use]
pub fn today_section(
    scheduled_at: Option<&SunriseTime>,
    due_at: Option<&SunriseTime>,
    now_ms: u64,
    tz: &str,
) -> TodaySection {
    sunrise_domain::today_section(scheduled_at, due_at, now_ms, &zone_or_utc(tz))
}

/// [`today_section`] over the planner days of `schedule`
/// (`docs/02-domain/day-schedule.md`): "today" and "overdue" are read against
/// the planner day holding `now_ms`, which is what `Query::Today` selects by.
/// A client with the vault's schedule calls this one.
#[must_use]
pub fn today_section_in(
    scheduled_at: Option<&SunriseTime>,
    due_at: Option<&SunriseTime>,
    now_ms: u64,
    tz: &str,
    schedule: &sunrise_domain::DaySchedule,
) -> TodaySection {
    sunrise_domain::today_section_in(scheduled_at, due_at, now_ms, &zone_or_utc(tz), schedule)
}

/// `relative_day`'s two answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelativeDay {
    /// `yesterday` / `today` / `tomorrow` / `+3d` / `-3d`, or `unknown` for a
    /// time kind this build does not know.
    pub text: String,
    /// Whether the day is behind `now_ms`.
    pub is_past: bool,
}

/// `yesterday` / `today` / `tomorrow` / `+3d`, in whole civil days in `tz`.
///
/// See [`sunrise_domain::relative_day`]. A kind this build does not know reads
/// as `unknown`, not as the day it happens to be indexed at (ADR-0045 §6),
/// and so does a value with no instant to measure from — never the epoch
/// (`docs/11-adr/0066-times-survive-travel.md` §3).
#[must_use]
pub fn relative_day(at: &SunriseTime, now_ms: u64, tz: &str) -> RelativeDay {
    let unknown = || RelativeDay {
        text: "unknown".to_owned(),
        is_past: false,
    };
    if let SunriseTime::Unknown { .. } = at {
        return unknown();
    }
    let zone = zone_or_utc(tz);
    let Ok(instant) = at.to_instant(&zone) else {
        return unknown();
    };
    let (text, is_past) = sunrise_domain::relative_day(instant, now_ms, &zone);
    RelativeDay { text, is_past }
}

/// A time value resolved to an instant in `tz`, in epoch milliseconds. A value
/// with no instant answers where a sort places it,
/// [`sunrise_domain::SunriseTime::instant_or_last`], never the epoch.
#[must_use]
pub fn time_value_ms(value: &SunriseTime, tz: &str) -> i64 {
    value.instant_or_last(&zone_or_utc(tz)).as_millisecond()
}

/// When `routine` next fires at or after `now_ms` for a reader in zone
/// `reader` (an IANA name; one that does not resolve reads as UTC), or `None`
/// if nothing falls inside its materialization horizon.
///
/// See [`sunrise_domain::routine_rows`], which is where the horizon lives: it
/// is per-frequency, so a yearly routine still resolves while a daily one
/// stays cheap. Skips, pauses and the series end are honoured, because the
/// whole routine is handed over rather than just its rule. A floating
/// routine fires at its wall-clock time in `reader`; a zoned one in its zone.
#[must_use]
pub fn next_occurrence_ms(routine: Routine, now_ms: u64, reader: &str) -> Option<i64> {
    let now = Timestamp::from_millisecond(i64::try_from(now_ms).ok()?).ok()?;
    sunrise_domain::routine_rows(&[routine], now, &zone_or_utc(reader))
        .first()
        .and_then(|r| r.next)
        .map(Timestamp::as_millisecond)
}

/// A running session's numbers at `now_ms`, as the domain derives them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionProgress {
    /// Focused time: frozen once ended, derived while running.
    pub focused_ms: u64,
    /// `MM:SS`, widening past an hour — the live-timer form.
    pub clock: String,
    /// Time left against the plan; absent for an `until done` session.
    pub remaining_ms: Option<u64>,
    /// `remaining_ms` as a clock, when there is one.
    pub remaining_clock: Option<String>,
    /// Whether a planned session has run past its plan.
    pub overran: bool,
    /// Still running (a `start` with no `end` — a valid state).
    pub running: bool,
}

/// What a live focus timer shows for `session` at `now_ms`.
///
/// The core stores no running timer: every number is one of
/// [`FocusSession`]'s methods, derived from the clock each time it is asked.
/// An open-ended session has no remaining time, which is not zero.
#[must_use]
pub fn session_progress(session: &FocusSession, now_ms: u64) -> SessionProgress {
    let focused_ms = session.focused_ms(now_ms);
    let remaining_ms = session.remaining_ms(now_ms);
    SessionProgress {
        focused_ms,
        clock: sunrise_domain::fmt_duration_ms(focused_ms),
        remaining_ms,
        remaining_clock: remaining_ms.map(sunrise_domain::fmt_duration_ms),
        overran: session.overran(now_ms),
        running: session.is_running(),
    }
}

/// A `sort_order` key between two adjacent siblings' keys; `None` on either
/// side is that end of the list. `None` back when the bounds admit no key
/// between them, which is a stale or non-adjacent read to redo rather than a
/// position to invent. See [`sunrise_domain::sort_order::between`].
#[must_use]
pub fn sort_key_between(after: Option<&str>, before: Option<&str>) -> Option<String> {
    sunrise_domain::sort_order::between(after, before).ok()
}

/// Every overlapping pair among `blocks`, each bound resolved in `tz`. See
/// [`sunrise_domain::overlaps`]; back-to-back blocks are not a conflict.
#[must_use]
pub fn block_conflicts(blocks: &[Block], tz: &str) -> Vec<BlockOverlap> {
    sunrise_domain::overlaps(blocks, &zone_or_utc(tz))
}

/// The draft the calendar's **Merge** action creates from two blocks, with
/// disagreeing time kinds resolved in `tz`. See
/// [`sunrise_domain::merge_blocks`].
///
/// # Errors
/// The union does not validate.
pub fn merged_block_draft(a: &Block, b: &Block, tz: &str) -> Result<BlockDraft, ValidationError> {
    sunrise_domain::merge_blocks(a, b, &zone_or_utc(tz))
}

/// When a snooze of `span` from `from_ms` lands, in epoch milliseconds, with
/// the civil arithmetic done in `tz`. See [`sunrise_domain::snooze_target`]:
/// "tomorrow" is a date, so adding 86,400,000 ms is wrong across a DST
/// boundary.
#[must_use]
pub fn snooze_target_ms(from_ms: u64, span: SnoozeSpan, tz: &str) -> i64 {
    sunrise_domain::snooze_target(instant_at(from_ms), span, &zone_or_utc(tz)).as_millisecond()
}

/// The key of the routine occurrence at `at`, for a routine in `tz` — what
/// [`sunrise_core::Command`]'s skip and completion commands name an
/// occurrence by. See [`sunrise_domain::occurrence_key_at`].
///
/// Unlike the rest of this module an unknown zone is an error, not UTC: a key
/// is an identity every replica must derive alike, and one derived in a
/// stand-in zone names a different occurrence.
///
/// # Errors
///
/// [`sunrise_domain::ExpandError::InvalidTimeZone`] for a zone the tzdb does
/// not know.
pub fn occurrence_key_at(tz: &str, at: Timestamp) -> Result<String, sunrise_domain::ExpandError> {
    sunrise_domain::occurrence_key_at(tz, at)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-03-07T12:00:00Z: the day before North America's spring-forward.
    const SAT_NOON_UTC: u64 = 1_772_884_800_000;

    #[test]
    fn an_unknown_zone_reads_as_utc() {
        assert_eq!(zone_or_utc("Not/AZone"), TimeZone::UTC);
        assert_ne!(zone_or_utc("America/Toronto"), TimeZone::UTC);
    }

    #[test]
    fn an_instant_out_of_range_reads_as_the_epoch() {
        assert_eq!(instant_at(0), Timestamp::UNIX_EPOCH);
        assert_eq!(instant_at(u64::MAX), Timestamp::UNIX_EPOCH);
        assert_eq!(
            instant_at(SAT_NOON_UTC).as_millisecond(),
            i64::try_from(SAT_NOON_UTC).unwrap()
        );
    }

    #[test]
    fn tomorrow_is_a_date_not_a_day_of_milliseconds() {
        let one_day = 86_400_000_i64;
        let utc = snooze_target_ms(SAT_NOON_UTC, SnoozeSpan::Tomorrow, "UTC");
        let toronto = snooze_target_ms(SAT_NOON_UTC, SnoozeSpan::Tomorrow, "America/Toronto");
        let from = i64::try_from(SAT_NOON_UTC).unwrap();
        assert_eq!(utc - from, one_day);
        assert_eq!(
            toronto - from,
            one_day - 3_600_000,
            "the same wall-clock time across spring-forward is 23 hours"
        );
        assert_eq!(
            snooze_target_ms(SAT_NOON_UTC, SnoozeSpan::OneHour, "America/Toronto") - from,
            3_600_000
        );
    }

    /// An occurrence's key is its wall clock in the routine's zone, and an
    /// unknown zone is refused rather than read as UTC.
    #[test]
    fn an_occurrence_key_is_read_in_the_routines_zone() {
        let at = instant_at(SAT_NOON_UTC);
        let utc = occurrence_key_at("UTC", at).unwrap();
        let toronto = occurrence_key_at("America/Toronto", at).unwrap();
        assert_ne!(utc, toronto);
        assert_eq!(utc, sunrise_domain::occurrence_key_at("UTC", at).unwrap());
        assert!(occurrence_key_at("Not/AZone", at).is_err());
    }

    #[test]
    fn an_unknown_kind_is_unknown_rather_than_a_day() {
        let unknown = SunriseTime::Unknown {
            kind: "lunar".into(),
            raw: sunrise_domain::Unknowns::default(),
        };
        assert_eq!(
            relative_day(&unknown, SAT_NOON_UTC, "UTC"),
            RelativeDay {
                text: "unknown".into(),
                is_past: false
            }
        );
    }

    /// A civil value past the timestamp range in the reader's zone has no
    /// instant: it is unknown and sorts last, never the epoch (ADR-0066 §3).
    #[test]
    fn a_value_with_no_instant_is_unknown_and_sorts_last() {
        let edge = SunriseTime::floating(jiff::civil::date(9999, 12, 31).at(23, 0, 0, 0));
        assert_eq!(
            relative_day(&edge, SAT_NOON_UTC, "Pacific/Honolulu"),
            RelativeDay {
                text: "unknown".into(),
                is_past: false
            }
        );
        let placed = time_value_ms(&edge, "Pacific/Honolulu");
        assert!(placed > i64::try_from(SAT_NOON_UTC).unwrap(), "{placed}");
        assert_eq!(
            placed,
            edge.instant_or_last(&zone_or_utc("Pacific/Honolulu"))
                .as_millisecond()
        );
    }

    #[test]
    fn relative_day_is_read_in_the_callers_zone() {
        let at = SunriseTime::Instant {
            at: instant_at(SAT_NOON_UTC + 86_400_000),
        };

        let day = relative_day(&at, SAT_NOON_UTC, "UTC");
        assert_eq!(day.text, "tomorrow");
        assert!(!day.is_past);
        assert_eq!(
            time_value_ms(&at, "Not/AZone"),
            i64::try_from(SAT_NOON_UTC).unwrap() + 86_400_000
        );
    }

    fn eref(kind: sunrise_id::EntityKind, n: u8) -> sunrise_id::EntityRef {
        sunrise_id::EntityRef::new(kind, [n; 16])
    }

    fn block(n: u8, from_ms: u64, to_ms: u64) -> Block {
        Block {
            id: eref(sunrise_id::EntityKind::Block, n),
            created_at: Timestamp::UNIX_EPOCH,
            updated_at: Timestamp::UNIX_EPOCH,
            stream_id: None,
            starts_at: SunriseTime::Instant {
                at: instant_at(from_ms),
            },
            ends_at: SunriseTime::Instant {
                at: instant_at(to_ms),
            },
            title: Some(format!("block {n}")),
            title_track_task: false,
            tasks: std::collections::BTreeSet::new(),
            deleted: false,
            unknown: sunrise_domain::Unknowns::new(),
        }
    }

    fn session(planned_ms: Option<u64>) -> FocusSession {
        FocusSession {
            start: sunrise_domain::FocusStart {
                id: eref(sunrise_id::EntityKind::FocusSession, 1),
                task_id: eref(sunrise_id::EntityKind::Task, 2),
                stream_id: sunrise_domain::inbox_stream_ref(),
                started_at: instant_at(SAT_NOON_UTC),
                planned_ms,
                energy: None,
                kind: sunrise_domain::FocusKind::Work,
                chunk: None,
                unknown: sunrise_domain::Unknowns::new(),
            },
            end: None,
            interruptions: Vec::new(),
        }
    }

    /// Overdue is "before today began where the reader is", not "before now":
    /// a task due at 09:00 is due, not overdue, when it is read at 17:00.
    #[test]
    fn overdue_starts_at_the_readers_midnight() {
        let floating = |day: i8, hour: i8| SunriseTime::Floating {
            civil: jiff::civil::date(2026, 3, day).at(hour, 0, 0, 0),
        };
        // 17:00 in Toronto on Saturday 2026-03-07.
        let now = SAT_NOON_UTC + 10 * 3_600_000;
        let tz = "America/Toronto";
        assert_eq!(
            today_section(None, Some(&floating(7, 9)), now, tz),
            TodaySection::Due
        );
        assert_eq!(
            today_section(None, Some(&floating(6, 23)), now, tz),
            TodaySection::Overdue
        );
    }

    /// With a 01:00 sleep, 00:30 is still yesterday's planner day: a task due
    /// yesterday is due, not overdue, until the next day begins.
    #[test]
    fn overdue_starts_at_the_planner_days_start_with_a_schedule() {
        let due = SunriseTime::AllDay {
            date: jiff::civil::date(2026, 3, 6),
        };
        let schedule = sunrise_domain::DaySchedule {
            weekday: [sunrise_domain::DayWindow::new(
                jiff::civil::time(7, 0, 0, 0),
                jiff::civil::time(1, 0, 0, 0),
            ); 7],
            ..sunrise_domain::DaySchedule::default()
        };
        // 00:30 in Toronto on Saturday 2026-03-07 (EST).
        let now = u64::try_from(
            "2026-03-07T05:30Z"
                .parse::<Timestamp>()
                .unwrap()
                .as_millisecond(),
        )
        .unwrap();
        let tz = "America/Toronto";
        assert_eq!(
            today_section(None, Some(&due), now, tz),
            TodaySection::Overdue,
            "civil midnight has passed"
        );
        assert_eq!(
            today_section_in(None, Some(&due), now, tz, &schedule),
            TodaySection::Due,
            "the planner day has not"
        );
    }

    #[test]
    fn back_to_back_blocks_do_not_conflict_and_overlapping_ones_do() {
        let hour = 3_600_000;
        let t = SAT_NOON_UTC;
        let touching = [block(1, t, t + hour), block(2, t + hour, t + 2 * hour)];
        assert!(block_conflicts(&touching, "UTC").is_empty());

        let overlapping = [block(1, t, t + 2 * hour), block(2, t + hour, t + 3 * hour)];
        let found = block_conflicts(&overlapping, "UTC");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].from_ms, i64::try_from(t + hour).unwrap());
        assert_eq!(found[0].to_ms, i64::try_from(t + 2 * hour).unwrap());

        let merged = merged_block_draft(&overlapping[0], &overlapping[1], "UTC").unwrap();
        assert_eq!(merged.starts_at, overlapping[0].starts_at);
        assert_eq!(merged.ends_at, overlapping[1].ends_at);
    }

    /// An open-ended session has no remaining time, which is not zero, and a
    /// planned one past its plan has overrun.
    #[test]
    fn a_sessions_numbers_are_derived_from_the_clock() {
        let ten_min = 600_000;
        let open = session_progress(&session(None), SAT_NOON_UTC + ten_min);
        assert_eq!(open.focused_ms, ten_min);
        assert_eq!(open.clock, "10:00");
        assert_eq!((open.remaining_ms, open.remaining_clock), (None, None));
        assert!(open.running && !open.overran);

        let planned = session_progress(&session(Some(ten_min / 2)), SAT_NOON_UTC + ten_min);
        assert_eq!(planned.remaining_ms, Some(0));
        assert!(planned.overran);
    }

    #[test]
    fn a_sort_key_falls_between_its_bounds_or_is_refused() {
        let k = sort_key_between(Some("B"), Some("D")).expect("a key between B and D");
        assert!("B" < k.as_str() && k.as_str() < "D", "{k}");
        assert!(sort_key_between(Some("C"), Some("B")).is_none());
    }
}

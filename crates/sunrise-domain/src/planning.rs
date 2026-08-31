//! Which section of the Today view a task belongs in.
//!
//! `docs/08-features/planning-views.md` composes Today out of named sections
//! and fixes the overdue boundary exactly: a task is overdue **iff** `due_at`
//! falls before the start of today in the reader's zone, "regardless of the
//! wall-clock time within the day". That is a domain rule, not a layout
//! choice, and it is not obvious enough to survive being re-derived: the
//! tempting `due_at < now` reads a task due at 17:00 as overdue from 17:01,
//! which the spec explicitly rules out.
//!
//! It lives here for the same reason [`crate::phrase`] does. Two clients that
//! each decide what "overdue" means will disagree, and the user will be right
//! to call the disagreement a bug.
//!
//! Deliberately **not** a query: the ordering *within* a section is the core's
//! (`Query::Today` sorts by `COALESCE(scheduled_at, due_at)`), and this only
//! says which bucket a row lands in. Classify, then stable-partition, and both
//! facts stay owned by whoever owns them.

use jiff::tz::TimeZone;
use jiff::Timestamp;

use crate::time::SunriseTime;

/// A section of the Today view, in the order the spec lists them.
///
/// The derived [`Ord`] is that order — sections sort the way they are
/// declared, so a client that sorts its groups gets the documented layout
/// without restating it. Blocks (§1) and promote-from-Inbox (§4) are not here:
/// neither is a property of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TodaySection {
    /// The user said they would do this today (§2). Also covers a task
    /// scheduled *before* today and still open: an intention that has slipped
    /// is still an intention, and the spec reserves "overdue" for deadlines.
    Scheduled,
    /// Due today, with no scheduled time of its own (§3).
    Due,
    /// The deadline passed before today began (§5). Folded by default, with a
    /// count badge.
    Overdue,
}

impl TodaySection {
    /// The section heading, as the spec words it.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Scheduled => "Scheduled",
            Self::Due => "Due today",
            Self::Overdue => "Overdue",
        }
    }
}

/// Which section a Today row belongs in.
///
/// `tz` is the reading device's zone, which is what resolves the kinds of
/// [`SunriseTime`] that carry none of their own — an all-day deadline is
/// "today" on the day it names, wherever it is read.
///
/// Total by construction. A task carrying neither time cannot reach Today at
/// all (the query selects on one or the other being set), and classifies as
/// [`TodaySection::Due`] rather than panicking if one ever does.
#[must_use]
pub fn today_section(
    scheduled_at: Option<&SunriseTime>,
    due_at: Option<&SunriseTime>,
    now_ms: u64,
    tz: &TimeZone,
) -> TodaySection {
    if due_at.is_some_and(|d| is_overdue(d, now_ms, tz)) {
        TodaySection::Overdue
    } else if scheduled_at.is_some() {
        TodaySection::Scheduled
    } else {
        TodaySection::Due
    }
}

/// `due_at < start_of_today_local`, compared as whole civil days.
///
/// Comparing dates rather than instants is the whole point: subtracting a
/// day's worth of milliseconds from `now` would make "overdue" depend on the
/// time of day, and days are not all the same length across a DST boundary
/// anyway.
#[must_use]
pub fn is_overdue(due_at: &SunriseTime, now_ms: u64, tz: &TimeZone) -> bool {
    let Ok(Ok(now)) = i64::try_from(now_ms).map(Timestamp::from_millisecond) else {
        // An unreadable clock is not evidence that anything is late.
        return false;
    };
    let today = now.to_zoned(tz.clone()).date();
    let due = due_at.to_instant(tz).to_zoned(tz.clone()).date();
    due < today
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil;

    /// 2026-03-10T17:00 in New York, as epoch ms.
    fn now_ny() -> u64 {
        let tz = TimeZone::get("America/New_York").expect("tz");
        u64::try_from(
            tz.to_zoned(civil::date(2026, 3, 10).at(17, 0, 0, 0))
                .expect("civil")
                .timestamp()
                .as_millisecond(),
        )
        .expect("positive")
    }

    fn ny() -> TimeZone {
        TimeZone::get("America/New_York").expect("tz")
    }

    #[test]
    fn a_deadline_later_today_is_not_overdue_even_after_it_passes() {
        // 09:00 today, read at 17:00 today. The spec is explicit that this is
        // a Today task, not an overdue one.
        let due = SunriseTime::zoned(civil::date(2026, 3, 10).at(9, 0, 0, 0), "America/New_York");
        assert!(!is_overdue(&due, now_ny(), &ny()));
        assert_eq!(
            today_section(None, Some(&due), now_ny(), &ny()),
            TodaySection::Due
        );
    }

    #[test]
    fn a_deadline_yesterday_is_overdue() {
        let due = SunriseTime::zoned(civil::date(2026, 3, 9).at(23, 59, 0, 0), "America/New_York");
        assert!(is_overdue(&due, now_ny(), &ny()));
        assert_eq!(
            today_section(None, Some(&due), now_ny(), &ny()),
            TodaySection::Overdue
        );
    }

    #[test]
    fn an_all_day_deadline_is_due_on_the_day_it_names() {
        let today = SunriseTime::all_day(civil::date(2026, 3, 10));
        let yesterday = SunriseTime::all_day(civil::date(2026, 3, 9));
        assert!(!is_overdue(&today, now_ny(), &ny()));
        assert!(is_overdue(&yesterday, now_ny(), &ny()));
    }

    #[test]
    fn the_reader_s_zone_decides_for_a_floating_deadline() {
        // A floating deadline carries no zone, so the reader supplies one and
        // the answer legitimately differs between two readers. 22:00 on the
        // 10th is still ahead in New York, where it is 17:00 on the 10th; the
        // same wall clock read in Tokyo — where that instant is already 06:00
        // on the 11th — is yesterday, and overdue.
        let late_today = SunriseTime::floating(civil::date(2026, 3, 10).at(22, 0, 0, 0));
        assert!(!is_overdue(&late_today, now_ny(), &ny()));
        let tokyo = TimeZone::get("Asia/Tokyo").expect("tz");
        assert!(is_overdue(&late_today, now_ny(), &tokyo));
    }

    #[test]
    fn an_overdue_deadline_outranks_a_scheduled_time() {
        // A task can be both scheduled for today and overdue; the folded
        // Overdue section is where the user needs to see it.
        let sched =
            SunriseTime::zoned(civil::date(2026, 3, 10).at(15, 0, 0, 0), "America/New_York");
        let due = SunriseTime::all_day(civil::date(2026, 3, 1));
        assert_eq!(
            today_section(Some(&sched), Some(&due), now_ny(), &ny()),
            TodaySection::Overdue
        );
    }

    #[test]
    fn a_scheduled_task_that_slipped_is_still_scheduled_not_overdue() {
        // Scheduled last week, no deadline. The spec's overdue boundary reads
        // `due_at` only.
        let sched = SunriseTime::all_day(civil::date(2026, 3, 2));
        assert_eq!(
            today_section(Some(&sched), None, now_ny(), &ny()),
            TodaySection::Scheduled
        );
    }

    #[test]
    fn the_sections_sort_the_way_the_spec_lists_them() {
        let mut all = [
            TodaySection::Overdue,
            TodaySection::Due,
            TodaySection::Scheduled,
        ];
        all.sort_unstable();
        assert_eq!(
            all,
            [
                TodaySection::Scheduled,
                TodaySection::Due,
                TodaySection::Overdue
            ]
        );
    }

    #[test]
    fn a_row_with_neither_time_classifies_rather_than_panicking() {
        assert_eq!(
            today_section(None, None, now_ny(), &ny()),
            TodaySection::Due
        );
    }

    #[test]
    fn an_unreadable_clock_does_not_make_everything_late() {
        let due = SunriseTime::all_day(civil::date(1999, 1, 1));
        assert!(!is_overdue(&due, u64::MAX, &ny()));
    }
}

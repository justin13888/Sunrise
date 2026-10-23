//! What a client decides around a review: the daily glance's window, the
//! snapshot it saves, and the name an export is filed under
//! (`docs/08-features/reviews-and-stats.md`).
//!
//! Every report is one core query; none of this recomputes one. What is here
//! is the arguments a client hands those queries and the record it writes
//! back, each of which the Apple client used to derive itself — the window as
//! `now − 24 h` and the export date in UTC, both of which name the wrong day
//! for a reader away from UTC or on a DST day.

use jiff::civil;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use sunrise_domain::{
    ExportDataset, ExportFormat, ReviewSnapshotDraft, ReviewSnapshotStream, ReviewTotals,
    ReviewWindow, StreakRow, Unknowns, WeeklyReview,
};
use sunrise_id::EntityRef;

/// How many weeks the trends cover until the reader picks another span.
pub const DEFAULT_TREND_WEEKS: u32 = 8;

/// How many saved snapshots the history shows.
pub const HISTORY_LIMIT: u32 = 50;

/// Where the daily glance starts: the start of the previous civil day in
/// `tz`, so "any captures from yesterday evening?" (spec §Daily review) is
/// answered whatever the hour.
///
/// Not `now − 24 h`. Read at 08:00 that misses everything captured yesterday
/// before 08:00, and on a DST day it is 23 or 25 hours from a different
/// midnight. This is the same boundary the morning summary draws
/// ([`sunrise_domain::MorningSummary::since`]), so the two views agree about
/// what "since yesterday" covers.
#[must_use]
pub fn daily_review_since(now: Timestamp, tz: &TimeZone) -> Timestamp {
    let today = now.to_zoned(tz.clone()).date();
    today
        .yesterday()
        .ok()
        .and_then(|d| tz.to_zoned(d.to_datetime(civil::Time::midnight())).ok())
        .map_or(now, |z| z.timestamp())
}

/// [`daily_review_since`], in the epoch milliseconds
/// `Query::DailyReview { since_ms, .. }` takes.
#[must_use]
pub fn daily_review_since_ms(now_ms: u64, tz: &TimeZone) -> u64 {
    let now = crate::vocab::instant_at(now_ms);
    u64::try_from(daily_review_since(now, tz).as_millisecond()).unwrap_or(0)
}

/// A snapshot's note: trimmed, and a blank one is no note.
#[must_use]
pub fn snapshot_note(note: Option<&str>) -> Option<String> {
    note.map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
}

/// One stream's counts as a review screen shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamTally {
    /// The stream.
    pub stream: EntityRef,
    /// Its name on screen.
    pub name: String,
    /// Completed in the window.
    pub completed: usize,
    /// Deferred in the window.
    pub deferred: usize,
    /// Created in the window and untouched since.
    pub created: usize,
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The snapshot of the review a screen showed, from what it showed.
///
/// A snapshot is the one fact about a review the op log cannot re-derive —
/// that someone did it, and what they saw — so it carries the screen's
/// counts rather than being recomputed when read. A count past `u32` is held
/// at the maximum rather than trapping.
#[must_use]
pub fn snapshot_draft(
    window: &ReviewWindow,
    totals: ReviewTotals,
    streams: Vec<StreamTally>,
    streaks: Vec<StreakRow>,
    note: Option<&str>,
) -> ReviewSnapshotDraft {
    ReviewSnapshotDraft {
        window_start_ms: window.start_ms,
        window_end_ms: window.end_ms,
        totals,
        streams: streams
            .into_iter()
            .map(|s| ReviewSnapshotStream {
                stream: s.stream,
                name: s.name,
                completed: count(s.completed),
                deferred: count(s.deferred),
                created: count(s.created),
                unknown: Unknowns::new(),
            })
            .collect(),
        streaks,
        note: snapshot_note(note),
    }
}

/// [`snapshot_draft`] of an assembled weekly review.
#[must_use]
pub fn snapshot_draft_of(review: &WeeklyReview, note: Option<&str>) -> ReviewSnapshotDraft {
    review.to_draft(snapshot_note(note))
}

/// `sunrise-<dataset>-<YYYY-MM-DD>.<format>`, dated by the civil day of
/// `now_ms` in `tz` — the day the reader exported on, not the UTC one.
#[must_use]
pub fn export_filename(
    dataset: ExportDataset,
    format: ExportFormat,
    now_ms: u64,
    tz: &TimeZone,
) -> String {
    let day = crate::vocab::instant_at(now_ms).to_zoned(tz.clone()).date();
    format!("sunrise-{}-{day}.{}", dataset.as_str(), format.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::task;
    use sunrise_domain::{FocusStats, StreamReview, Trends};

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn empty_focus() -> FocusStats {
        FocusStats {
            sessions: 0,
            work_sessions: 0,
            running: 0,
            total_focused_ms: 0,
            interruptions: 0,
            per_stream: vec![],
            per_energy: vec![],
            overall: None,
            top_interruptions: vec![],
        }
    }

    fn empty_trends() -> Trends {
        Trends {
            week_starts: vec![],
            overall: vec![],
            per_stream: vec![],
        }
    }

    /// 08:00 in Toronto looks back to Toronto's yesterday midnight, not to
    /// 08:00 yesterday.
    #[test]
    fn the_daily_glance_starts_at_yesterdays_midnight_in_the_readers_zone() {
        let toronto = TimeZone::get("America/Toronto").unwrap();
        let now = ts("2026-06-02T12:00:00Z"); // 08:00 EDT
        assert_eq!(
            daily_review_since(now, &toronto),
            ts("2026-06-01T04:00:00Z")
        );
        assert_eq!(
            daily_review_since(now, &TimeZone::UTC),
            ts("2026-06-01T00:00:00Z")
        );
    }

    /// The day after spring-forward: yesterday was 23 hours long, and the
    /// window still opens at its midnight.
    #[test]
    fn a_dst_day_is_still_a_day() {
        let ny = TimeZone::get("America/New_York").unwrap();
        let now = ts("2026-03-09T13:00:00Z"); // Monday 09:00 EDT
        let since = daily_review_since(now, &ny);
        assert_eq!(since, ts("2026-03-08T05:00:00Z"), "Sunday 00:00 EST");
        assert_eq!(
            daily_review_since_ms(u64::try_from(now.as_millisecond()).unwrap(), &ny),
            u64::try_from(since.as_millisecond()).unwrap()
        );
    }

    #[test]
    fn a_blank_note_is_no_note() {
        assert_eq!(snapshot_note(Some("   ")), None);
        assert_eq!(snapshot_note(None), None);
        assert_eq!(
            snapshot_note(Some(" quiet week ")).as_deref(),
            Some("quiet week")
        );
    }

    /// The draft built from a screen's tallies is the one the domain builds
    /// from the review itself.
    #[test]
    fn the_snapshot_records_the_counts_the_screen_showed() {
        let stream = task(1, 7).stream_id;
        let review = WeeklyReview {
            window: ReviewWindow::new(1_000, 2_000),
            streams: vec![StreamReview {
                stream,
                name: "Work".into(),
                completed: vec![task(1, 7), task(2, 7)],
                deferred: vec![task(3, 7)],
                created_untouched: vec![],
                focus: None,
                trend: vec![],
            }],
            inbox: vec![],
            drifting_routines: vec![],
            streaks: vec![],
            slipped: vec![],
            totals: ReviewTotals::default(),
            focus: empty_focus(),
            trends: empty_trends(),
        };
        let tallies = vec![StreamTally {
            stream,
            name: "Work".into(),
            completed: 2,
            deferred: 1,
            created: 0,
        }];
        let from_screen = snapshot_draft(
            &review.window,
            review.totals.clone(),
            tallies,
            vec![],
            Some(" quiet "),
        );
        assert_eq!(from_screen, snapshot_draft_of(&review, Some("quiet")));
        assert_eq!(from_screen.streams[0].completed, 2);
        assert_eq!(from_screen.note.as_deref(), Some("quiet"));
    }

    /// 23:30 in Toronto on the 1st is the 2nd in UTC; the file says the 1st.
    #[test]
    fn an_export_is_dated_by_the_readers_day() {
        let toronto = TimeZone::get("America/Toronto").unwrap();
        let now = u64::try_from(ts("2026-06-02T03:30:00Z").as_millisecond()).unwrap();
        assert_eq!(
            export_filename(ExportDataset::Trends, ExportFormat::Csv, now, &toronto),
            "sunrise-trends-2026-06-01.csv"
        );
        assert_eq!(
            export_filename(
                ExportDataset::Trends,
                ExportFormat::Json,
                now,
                &TimeZone::UTC
            ),
            "sunrise-trends-2026-06-02.json"
        );
    }
}

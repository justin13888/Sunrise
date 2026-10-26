//! The daily briefs and the review's client-side arguments across the seam:
//! [`sunrise_client_core::brief`] and [`sunrise_client_core::review`],
//! mirrored, and the routine occurrence key.
//!
//! The brief methods read their report and render it in one call, in the
//! reader's zone the core read it in, so a client never splits a list on a
//! boundary of its own.

use sunrise_client_core::{brief, review};
use sunrise_core::{Query, QueryResult};
use sunrise_domain::{ExportDataset, ExportFormat, ReviewWindow, SnoozeSpan};

use crate::dto::{
    DailyReviewReport, EveningReport, MorningReport, SnapshotDraftIn, SnapshotStream, StreakEntry,
    TaskItem, WeeklyReviewReport,
};
use crate::editing::zone;
use crate::{BindingError, SunriseCore};

/// One section of a brief: a stable id, a heading, what it says when empty,
/// and its rows. An empty section still renders.
#[derive(Debug, Clone, uniffi::Record)]
pub struct BriefSectionItem {
    /// `yesterday`, `today`, `triage`, `due`, `open`, `week` or `backlog`.
    pub id: String,
    /// The heading.
    pub title: String,
    /// Shown under the heading when there are no rows.
    pub empty_message: String,
    /// The rows, in the report's order.
    pub tasks: Vec<TaskItem>,
}

impl From<brief::BriefSection> for BriefSectionItem {
    fn from(s: brief::BriefSection) -> Self {
        Self {
            id: s.id.as_str().to_owned(),
            title: s.title.to_owned(),
            empty_message: s.empty_message.to_owned(),
            tasks: s.tasks.iter().map(TaskItem::from).collect(),
        }
    }
}

/// The morning summary, read and rendered.
#[derive(Debug, Clone, uniffi::Record)]
pub struct MorningBrief {
    /// The report as the core assembled it.
    pub report: MorningReport,
    /// Its sections, in the order the morning reads.
    pub sections: Vec<BriefSectionItem>,
    /// The one line the header says.
    pub headline: String,
}

/// The end-of-day plan, read and rendered.
#[derive(Debug, Clone, uniffi::Record)]
pub struct EveningBrief {
    /// The report as the core assembled it.
    pub report: EveningReport,
    /// Its sections.
    pub sections: Vec<BriefSectionItem>,
    /// The one line the header says.
    pub headline: String,
}

/// Which brief.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum BriefKind {
    /// The morning summary.
    Morning,
    /// The end-of-day plan.
    Evening,
}

fn shape(what: &str) -> BindingError {
    BindingError::Core(format!("{what} read returned another shape"))
}

#[uniffi::export(async_runtime = "tokio")]
impl SunriseCore {
    /// `Query::MorningSummary`, split into its sections in the reader's zone.
    pub async fn morning_brief(&self, now_ms: u64) -> Result<MorningBrief, BindingError> {
        let QueryResult::MorningSummary(s) =
            self.inner.query(Query::MorningSummary { now_ms }).await?
        else {
            return Err(shape("morning summary"));
        };
        let zone = self.inner.reader_zone();
        Ok(MorningBrief {
            report: MorningReport::from(&*s),
            sections: brief::morning_sections(&s, &zone)
                .into_iter()
                .map(Into::into)
                .collect(),
            headline: brief::morning_headline(Some(&s)),
        })
    }

    /// `Query::EndOfDayPlan`, in its sections.
    pub async fn evening_brief(&self, now_ms: u64) -> Result<EveningBrief, BindingError> {
        let QueryResult::EndOfDayPlan(p) = self.inner.query(Query::EndOfDayPlan { now_ms }).await?
        else {
            return Err(shape("end-of-day plan"));
        };
        Ok(EveningBrief {
            report: EveningReport::from(&*p),
            sections: brief::evening_sections(&p)
                .into_iter()
                .map(Into::into)
                .collect(),
            headline: brief::evening_headline(Some(&p)),
        })
    }

    /// The daily glance, over the window from the start of the previous
    /// civil day in the reader's zone to `now_ms` — not `now − 24 h`. See
    /// [`sunrise_client_core::review::daily_review_since`].
    pub async fn daily_review(&self, now_ms: u64) -> Result<DailyReviewReport, BindingError> {
        let since_ms = review::daily_review_since_ms(now_ms, &self.inner.reader_zone());
        let QueryResult::DailyReview(r) = self
            .inner
            .query(Query::DailyReview { since_ms, now_ms })
            .await?
        else {
            return Err(shape("daily review"));
        };
        Ok(DailyReviewReport::from(&*r))
    }

    /// The export file name for `dataset` as `format`, dated by the reader's
    /// civil day. See [`sunrise_client_core::review::export_filename`].
    pub fn export_filename(
        &self,
        dataset: ExportDataset,
        format: ExportFormat,
        now_ms: u64,
    ) -> String {
        review::export_filename(dataset, format, now_ms, &self.inner.reader_zone())
    }
}

/// What a brief's header says before its first read.
#[uniffi::export]
#[must_use]
pub fn brief_loading_headline(kind: BriefKind) -> String {
    match kind {
        BriefKind::Morning => brief::morning_headline(None),
        BriefKind::Evening => brief::evening_headline(None),
    }
}

/// The spans a "not now" menu offers, in order.
#[uniffi::export]
#[must_use]
pub fn offered_snoozes() -> Vec<SnoozeSpan> {
    brief::OFFERED_SNOOZES.to_vec()
}

/// What a snooze button says.
#[uniffi::export]
#[must_use]
pub fn snooze_label(span: SnoozeSpan) -> String {
    brief::snooze_label(span).to_owned()
}

/// Where the daily glance starts for a reader in `tz`: the start of the
/// previous civil day.
///
/// # Errors
///
/// [`BindingError::BadTime`] for a zone the tzdb does not know.
#[uniffi::export]
pub fn daily_review_since_ms(now_ms: u64, tz: String) -> Result<u64, BindingError> {
    Ok(review::daily_review_since_ms(now_ms, &zone(&tz)?))
}

/// The snapshot of the weekly review a screen showed, with `note` trimmed
/// and a blank one dropped. Counts come from the lists the screen showed.
#[uniffi::export]
#[must_use]
pub fn review_snapshot_draft(report: WeeklyReviewReport, note: Option<String>) -> SnapshotDraftIn {
    let window = ReviewWindow::new(report.window.start_ms, report.window.end_ms);
    let streams = report
        .streams
        .into_iter()
        .map(|s| review::StreamTally {
            stream: s.stream,
            name: s.name,
            completed: s.completed.len(),
            deferred: s.deferred.len(),
            created: s.created_untouched.len(),
        })
        .collect();
    let d = review::snapshot_draft(
        &window,
        report.totals.into(),
        streams,
        report.streaks.into_iter().map(Into::into).collect(),
        note.as_deref(),
    );
    SnapshotDraftIn {
        window_start_ms: d.window_start_ms,
        window_end_ms: d.window_end_ms,
        totals: (&d.totals).into(),
        streams: d.streams.iter().map(SnapshotStream::from).collect(),
        streaks: d.streaks.iter().map(StreakEntry::from).collect(),
        note: d.note,
    }
}

/// The key of the routine occurrence at `at_ms`, for a routine in `tz`: what
/// skip and completion commands name an occurrence by. See
/// [`sunrise_domain::occurrence_key_at`].
///
/// # Errors
///
/// [`BindingError::BadTime`] for an instant out of range or a zone the tzdb
/// does not know — never a key derived in a stand-in zone.
#[uniffi::export]
pub fn occurrence_key_at(tz: String, at_ms: i64) -> Result<String, BindingError> {
    let at = jiff::Timestamp::from_millisecond(at_ms).map_err(|e| BindingError::BadTime {
        value: at_ms.to_string(),
        cause: e.to_string(),
    })?;
    sunrise_client_core::vocab::occurrence_key_at(&tz, at).map_err(|e| BindingError::BadTime {
        value: tz,
        cause: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::{FocusTotals, ReviewCounts, ReviewPeriod, StreamReviewRow, TrendReport};

    #[test]
    fn the_snooze_menu_and_placeholders_are_the_client_layers() {
        assert_eq!(
            offered_snoozes(),
            [
                SnoozeSpan::OneHour,
                SnoozeSpan::Tomorrow,
                SnoozeSpan::NextWeek
            ]
        );
        assert_eq!(snooze_label(SnoozeSpan::OneHour), "Defer 1 Hour");
        assert_eq!(
            brief_loading_headline(BriefKind::Morning),
            "Reading your morning…"
        );
    }

    #[test]
    fn the_glance_window_and_occurrence_key_refuse_an_unknown_zone() {
        assert!(daily_review_since_ms(0, "Not/AZone".into()).is_err());
        assert!(occurrence_key_at("Not/AZone".into(), 0).is_err());
        // 2026-06-02T12:00Z → 2026-06-01T00:00Z.
        assert_eq!(
            daily_review_since_ms(1_780_401_600_000, "UTC".into()).unwrap(),
            1_780_272_000_000
        );
        assert!(occurrence_key_at("UTC".into(), 1_780_272_000_000).is_ok());
    }

    #[test]
    fn a_snapshot_draft_counts_what_the_screen_listed() {
        let stream = sunrise_id::EntityRef::new(sunrise_id::EntityKind::Stream, [1; 16]);
        let report = WeeklyReviewReport {
            window: ReviewPeriod {
                start_ms: 1,
                end_ms: 2,
            },
            streams: vec![StreamReviewRow {
                stream,
                name: "Work".into(),
                completed: vec![],
                deferred: vec![],
                created_untouched: vec![],
                focus: None,
                trend: vec![],
            }],
            inbox: vec![],
            drifting_routines: vec![],
            streaks: vec![],
            slipped: vec![],
            totals: ReviewCounts::from(&sunrise_domain::ReviewTotals::default()),
            focus: FocusTotals::from(&sunrise_domain::FocusStats {
                sessions: 0,
                work_sessions: 0,
                running: 0,
                total_focused_ms: 0,
                interruptions: 0,
                per_stream: vec![],
                per_energy: vec![],
                overall: None,
                top_interruptions: vec![],
            }),
            trends: TrendReport::from(&sunrise_domain::Trends {
                week_starts: vec![],
                overall: vec![],
                per_stream: vec![],
            }),
        };
        let d = review_snapshot_draft(report, Some("  ".into()));
        assert_eq!((d.window_start_ms, d.window_end_ms), (1, 2));
        assert_eq!(d.streams.len(), 1);
        assert_eq!(d.streams[0].completed, 0);
        assert_eq!(d.note, None);
    }
}

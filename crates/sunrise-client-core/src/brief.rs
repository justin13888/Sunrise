//! The two daily briefs as a reader sees them: sections, a headline, and the
//! "not now" menu (`docs/08-features/notifications.md` §Morning brief).
//!
//! The core assembles each report in one read
//! ([`sunrise_core::Query::MorningSummary`],
//! [`sunrise_core::Query::EndOfDayPlan`]); the client's job is to render it,
//! not to recompute it. What rendering still decides — the order of the
//! sections, what an empty one says, where "yesterday" ends — is decided here
//! once, so every client says the same thing about the same morning.

use jiff::tz::TimeZone;
use sunrise_domain::{EndOfDayPlan, MorningSummary, SnoozeSpan, Task};

/// Which section of a brief.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BriefSectionId {
    /// Morning: finished on the previous civil day.
    Yesterday,
    /// Morning: already finished today.
    Today,
    /// Morning: the Inbox triage queue.
    Triage,
    /// Morning: scheduled or due today.
    Due,
    /// Evening: still open today, overdue included.
    Open,
    /// Evening: the seven civil days ahead.
    Week,
    /// Evening: open with no date at all.
    Backlog,
}

impl BriefSectionId {
    /// A stable name for the section, for a view's identity.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Yesterday => "yesterday",
            Self::Today => "today",
            Self::Triage => "triage",
            Self::Due => "due",
            Self::Open => "open",
            Self::Week => "week",
            Self::Backlog => "backlog",
        }
    }
}

/// One section: a heading, what it says when empty, and its rows.
///
/// An empty section still renders: "nothing to triage" is the answer someone
/// opened the view for.
#[derive(Debug, Clone, PartialEq)]
pub struct BriefSection {
    /// Which section.
    pub id: BriefSectionId,
    /// The heading.
    pub title: &'static str,
    /// Shown under the heading when there are no rows.
    pub empty_message: &'static str,
    /// The rows, in the report's order.
    pub tasks: Vec<Task>,
}

fn section(
    id: BriefSectionId,
    title: &'static str,
    empty_message: &'static str,
    tasks: Vec<Task>,
) -> BriefSection {
    BriefSection {
        id,
        title,
        empty_message,
        tasks,
    }
}

/// The morning, in the order it reads: the day that closed, then what is
/// already done today, then the two things that need a decision.
///
/// `completed` is split on [`MorningSummary::today_start`], with each
/// completion read in `tz`, the reader's zone — the zone the core drew that
/// boundary in.
#[must_use]
pub fn morning_sections(summary: &MorningSummary, tz: &TimeZone) -> Vec<BriefSection> {
    let (today, yesterday): (Vec<Task>, Vec<Task>) =
        summary.completed.iter().cloned().partition(|t| {
            t.completed_at
                .as_ref()
                .and_then(|at| at.to_instant(tz).ok())
                .is_some_and(|at| at >= summary.today_start)
        });
    vec![
        section(
            BriefSectionId::Yesterday,
            "Finished yesterday",
            "Nothing was completed yesterday.",
            yesterday,
        ),
        section(
            BriefSectionId::Today,
            "Already done today",
            "The day is still ahead of you.",
            today,
        ),
        section(
            BriefSectionId::Triage,
            "To triage",
            "Your Inbox is empty.",
            summary.to_triage.clone(),
        ),
        section(
            BriefSectionId::Due,
            "Landing today",
            "Nothing is scheduled or due today.",
            summary.due_today.clone(),
        ),
    ]
}

/// The morning's one line; a placeholder before the first read.
#[must_use]
pub fn morning_headline(summary: Option<&MorningSummary>) -> String {
    let Some(s) = summary else {
        return "Reading your morning…".to_owned();
    };
    let mut parts = vec![format!("{} completed since yesterday", s.completed.len())];
    if !s.due_today.is_empty() {
        parts.push(format!("{} landing today", s.due_today.len()));
    }
    if !s.to_triage.is_empty() {
        parts.push(format!("{} to triage", s.to_triage.len()));
    }
    parts.join(" · ")
}

/// The evening: what is left today, the week ahead, and the undated backlog
/// to plan from.
#[must_use]
pub fn evening_sections(plan: &EndOfDayPlan) -> Vec<BriefSection> {
    vec![
        section(
            BriefSectionId::Open,
            "Still open today",
            "Today is clear. Nothing was left behind.",
            plan.still_open.clone(),
        ),
        section(
            BriefSectionId::Week,
            "The week ahead",
            "Nothing is scheduled in the next seven days.",
            plan.week_ahead.clone(),
        ),
        section(
            BriefSectionId::Backlog,
            "Unscheduled",
            "Everything open has a date on it.",
            plan.unscheduled.clone(),
        ),
    ]
}

/// The evening's one line; a placeholder before the first read.
#[must_use]
pub fn evening_headline(plan: Option<&EndOfDayPlan>) -> String {
    let Some(p) = plan else {
        return "Reading your evening…".to_owned();
    };
    let mut parts = vec![format!("{} still open today", p.still_open.len())];
    if !p.week_ahead.is_empty() {
        parts.push(format!("{} this week", p.week_ahead.len()));
    }
    if !p.unscheduled.is_empty() {
        parts.push(format!("{} unscheduled", p.unscheduled.len()));
    }
    parts.join(" · ")
}

/// The spans a "not now" menu offers, in order. One list, so a snooze in the
/// evening view and one on a notification cannot mean different things.
pub const OFFERED_SNOOZES: [SnoozeSpan; 3] = [
    SnoozeSpan::OneHour,
    SnoozeSpan::Tomorrow,
    SnoozeSpan::NextWeek,
];

/// What a snooze button says.
#[must_use]
pub const fn snooze_label(span: SnoozeSpan) -> &'static str {
    match span {
        SnoozeSpan::OneHour => "Defer 1 Hour",
        SnoozeSpan::Tomorrow => "Snooze Until Tomorrow",
        SnoozeSpan::NextWeek => "Snooze Until Next Week",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::task;
    use jiff::Timestamp;
    use sunrise_domain::SunriseTime;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn done_at(n: u8, at: &str) -> Task {
        let mut t = task(n, 1);
        t.completed_at = Some(SunriseTime::instant(ts(at)));
        t
    }

    fn morning(completed: Vec<Task>, to_triage: Vec<Task>, due_today: Vec<Task>) -> MorningSummary {
        MorningSummary {
            since: ts("2026-03-01T05:00:00Z"),
            today_start: ts("2026-03-02T05:00:00Z"),
            completed,
            to_triage,
            due_today,
        }
    }

    /// An empty morning still shows every section, each saying so.
    #[test]
    fn an_empty_morning_still_shows_every_section() {
        let sections = morning_sections(&morning(vec![], vec![], vec![]), &TimeZone::UTC);
        let ids: Vec<_> = sections.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["yesterday", "today", "triage", "due"]);
        assert!(sections
            .iter()
            .all(|s| s.tasks.is_empty() && !s.empty_message.is_empty()));
    }

    /// Anything finished since midnight in the reader's zone is today's.
    #[test]
    fn a_task_completed_this_morning_is_separated_from_yesterdays() {
        let toronto = TimeZone::get("America/Toronto").unwrap();
        let summary = morning(
            vec![
                done_at(1, "2026-03-02T06:00:00Z"), // 01:00 Toronto, today
                done_at(2, "2026-03-02T04:00:00Z"), // 23:00 Toronto, yesterday
            ],
            vec![],
            vec![],
        );
        let sections = morning_sections(&summary, &toronto);
        assert_eq!(
            sections[0].tasks.iter().map(|t| t.id).collect::<Vec<_>>(),
            [task(2, 1).id]
        );
        assert_eq!(
            sections[1].tasks.iter().map(|t| t.id).collect::<Vec<_>>(),
            [task(1, 1).id]
        );
    }

    #[test]
    fn the_morning_headline_always_says_something() {
        assert_eq!(morning_headline(None), "Reading your morning…");
        assert_eq!(
            morning_headline(Some(&morning(vec![], vec![], vec![]))),
            "0 completed since yesterday"
        );
        assert_eq!(
            morning_headline(Some(&morning(vec![], vec![task(1, 1)], vec![task(2, 1)]))),
            "0 completed since yesterday · 1 landing today · 1 to triage"
        );
    }

    #[test]
    fn the_evening_has_three_sections_and_a_headline() {
        let plan = EndOfDayPlan {
            day_start: ts("2026-03-02T00:00:00Z"),
            day_end: ts("2026-03-03T00:00:00Z"),
            week_end: ts("2026-03-10T00:00:00Z"),
            still_open: vec![],
            week_ahead: vec![task(1, 1)],
            unscheduled: vec![task(2, 1), task(3, 1)],
        };
        let ids: Vec<_> = evening_sections(&plan)
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(ids, ["open", "week", "backlog"]);
        assert_eq!(
            evening_headline(Some(&plan)),
            "0 still open today · 1 this week · 2 unscheduled"
        );
        assert_eq!(evening_headline(None), "Reading your evening…");
    }

    #[test]
    fn the_offered_spans_are_the_domains_own() {
        assert_eq!(
            OFFERED_SNOOZES,
            [
                SnoozeSpan::OneHour,
                SnoozeSpan::Tomorrow,
                SnoozeSpan::NextWeek
            ]
        );
        assert_eq!(snooze_label(SnoozeSpan::Tomorrow), "Snooze Until Tomorrow");
    }
}

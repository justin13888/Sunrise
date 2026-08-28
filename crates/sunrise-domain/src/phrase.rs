//! How the domain says things out loud.
//!
//! Every one of these turns a domain value into the words a human reads for
//! it: a duration, a relative day, why the planner ranked something where it
//! did, what an activity event was. They are prose, not serialization —
//! nothing here round-trips, and nothing here should be parsed back.
//!
//! They live in the domain rather than in a client because the alternative is
//! each client inventing its own vocabulary. "unblocks 3 tasks" and "3 blocked
//! by this" are the same fact worded two ways, and a user moving between the
//! CLI and the app should not have to translate. The rendering — colour,
//! width, truncation — stays with whoever is drawing.
//!
//! Locale is deliberately out of scope in v1: these are English, and
//! localisation is a later, whole-product decision rather than a per-string
//! one.

use crate::activity::ActivityKind;
use crate::common::Energy;
use crate::constraint::{ConstraintSeverity, ScheduleConstraint};
use crate::focus::{EnergyFit, SessionLength, SessionPlan};

/// `MM:SS`, widening to `H:MM:SS` past an hour.
///
/// Used for every duration a focus surface shows, so a live timer and a folded
/// total read the same way.
#[must_use]
pub fn fmt_duration_ms(ms: u64) -> String {
    let total_s = ms / 1000;
    let (h, m, s) = (total_s / 3600, (total_s % 3600) / 60, total_s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// `1800` → `30m`, `5400` → `1h30`.
///
/// The compact form, for a column in a list. [`fmt_duration_ms`] is the form
/// for a timer.
#[must_use]
pub fn short_duration(secs: u64) -> String {
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    match (h, m) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h{m:02}"),
    }
}

/// Human label for an energy budget; `None` reads as "any", the value that
/// drops energy out of the planner ranking.
#[must_use]
pub const fn energy_budget_label(e: Option<Energy>) -> &'static str {
    match e {
        None => "any",
        Some(Energy::Low) => "low",
        Some(Energy::Med) => "med",
        Some(Energy::High) => "high",
    }
}

/// Human label for a session-length choice.
#[must_use]
pub const fn length_label(l: SessionLength) -> &'static str {
    match l {
        SessionLength::OnePomodoro => "one pomodoro",
        SessionLength::SizedToEstimate => "sized to estimate",
        SessionLength::UntilDone => "until done",
    }
}

/// Label for an energy fit, in the ranking's own order (best first).
#[must_use]
pub const fn energy_fit_label(fit: EnergyFit) -> &'static str {
    match fit {
        EnergyFit::Exact => "exact",
        EnergyFit::Unknown => "unknown",
        EnergyFit::Under => "under",
        EnergyFit::Over => "over",
    }
}

/// Why a planner row sits where it does: its leverage and its energy fit — the
/// two keys [`crate::focus::rank_focus_plan`] sorts on — plus the session that
/// would open.
///
/// Takes the three ranked facets rather than a query row so it stays in the
/// domain: the ranking is domain logic, and so is the sentence explaining it.
#[must_use]
pub fn plan_reason(unblocks: u32, energy_fit: EnergyFit, suggested: &SessionPlan) -> String {
    let mut parts = vec![match unblocks {
        0 => "unblocks nothing".to_string(),
        1 => "unblocks 1 task".to_string(),
        n => format!("unblocks {n} tasks"),
    }];
    parts.push(format!("fit {}", energy_fit_label(energy_fit)));
    parts.push(match suggested.planned_ms {
        Some(ms) => fmt_duration_ms(ms),
        None => "until done".into(),
    });
    if let Some(c) = suggested.chunk {
        parts.push(format!("chunk {} of {}", c.index, c.total));
    }
    parts.join(" · ")
}

/// One-line scheduling-constraints summary, e.g. `2 constraints (1 hard)`.
#[must_use]
pub fn constraint_summary(list: &[ScheduleConstraint]) -> String {
    let hard = list
        .iter()
        .filter(|c| c.severity == ConstraintSeverity::Hard)
        .count();
    let noun = if list.len() == 1 {
        "constraint"
    } else {
        "constraints"
    };
    format!("{} {noun} ({hard} hard)", list.len())
}

/// Human phrasing for one activity event.
///
/// Edits are *summarized* rather than enumerated, per
/// `docs/08-features/reviews-and-stats.md`: a feed that lists every field
/// touched stops being skimmable at the first bulk operation.
#[must_use]
pub fn activity_phrase(kind: &ActivityKind) -> String {
    use ActivityKind as K;
    match kind {
        K::TaskCreated => "created".into(),
        K::TaskCompleted => "completed".into(),
        K::TaskReopened => "reopened".into(),
        K::TaskCancelled => "cancelled".into(),
        K::TaskDeferred { count } => format!("deferred (#{count})"),
        K::TaskMoved { .. } => "moved stream".into(),
        K::TaskUpdated { fields: 1 } => "updated 1 field".into(),
        K::TaskUpdated { fields } => format!("updated {fields} fields"),
        K::TaskDeleted => "deleted".into(),
        K::StreamCreated => "stream created".into(),
        K::StreamDeleted => "stream deleted".into(),
        K::FocusStarted { planned_ms, .. } => match planned_ms {
            Some(ms) => format!("focus started ({})", fmt_duration_ms(*ms)),
            None => "focus started".into(),
        },
        K::FocusEnded {
            focused_ms,
            completed_task,
            ..
        } => {
            let tail = if *completed_task { ", completed" } else { "" };
            format!("focus ended ({}{tail})", fmt_duration_ms(*focused_ms))
        }
    }
}

/// `yesterday` / `today` / `tomorrow` / `+3d`, plus whether it is in the past.
///
/// Whole civil days in the caller's zone, not elapsed hours: "due tomorrow"
/// must not read as "today" because it happens to be 23:30 now. `now_ms` comes
/// from the caller's clock, so this stays pure.
#[must_use]
pub fn relative_day(at: jiff::Timestamp, now_ms: u64, tz: &jiff::tz::TimeZone) -> (String, bool) {
    let Ok(Ok(now)) = i64::try_from(now_ms).map(jiff::Timestamp::from_millisecond) else {
        return (String::new(), false);
    };
    let today = now.to_zoned(tz.clone()).date();
    let then = at.to_zoned(tz.clone()).date();
    let days = then.since(today).map(|d| d.get_days()).unwrap_or(0);
    let text = match days {
        0 => "today".to_string(),
        1 => "tomorrow".to_string(),
        -1 => "yesterday".to_string(),
        d if d < 0 => format!("{d}d"),
        d => format!("+{d}d"),
    };
    (text, days < 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constraint::{TimeOfDayRange, WeekdaySet};
    use crate::focus::Chunk;

    #[test]
    fn a_timer_widens_only_once_it_has_to() {
        assert_eq!(fmt_duration_ms(0), "00:00");
        assert_eq!(fmt_duration_ms(90_000), "01:30");
        assert_eq!(fmt_duration_ms(3_600_000), "1:00:00");
        assert_eq!(fmt_duration_ms(3_661_000), "1:01:01");
    }

    #[test]
    fn the_compact_duration_drops_the_zero_half() {
        assert_eq!(short_duration(1800), "30m");
        assert_eq!(short_duration(7200), "2h");
        assert_eq!(short_duration(5400), "1h30");
        // Two digits on the minutes so a column of them lines up.
        assert_eq!(short_duration(3900), "1h05");
    }

    #[test]
    fn a_planner_reason_names_both_ranking_keys() {
        let plan = SessionPlan {
            planned_ms: Some(1_500_000),
            chunk: None,
        };
        let s = plan_reason(3, EnergyFit::Exact, &plan);
        assert!(s.contains("unblocks 3 tasks"), "{s}");
        assert!(s.contains("fit exact"), "{s}");
        assert!(s.contains("25:00"), "{s}");
    }

    #[test]
    fn a_planner_reason_counts_in_words_a_human_would_use() {
        let plan = SessionPlan {
            planned_ms: None,
            chunk: None,
        };
        assert!(plan_reason(0, EnergyFit::Unknown, &plan).contains("unblocks nothing"));
        assert!(plan_reason(1, EnergyFit::Unknown, &plan).contains("unblocks 1 task"));
        // An open-ended session says so rather than showing 00:00.
        assert!(plan_reason(1, EnergyFit::Unknown, &plan).contains("until done"));
    }

    #[test]
    fn a_chunked_session_says_which_chunk() {
        let plan = SessionPlan {
            planned_ms: Some(1_500_000),
            chunk: Some(Chunk { index: 3, total: 4 }),
        };
        assert!(plan_reason(0, EnergyFit::Over, &plan).contains("chunk 3 of 4"));
    }

    #[test]
    fn the_constraint_summary_pluralizes_and_counts_the_hard_ones() {
        let soft = window();
        let hard = ScheduleConstraint {
            severity: ConstraintSeverity::Hard,
            ..window()
        };
        assert_eq!(constraint_summary(&[]), "0 constraints (0 hard)");
        assert_eq!(
            constraint_summary(std::slice::from_ref(&soft)),
            "1 constraint (0 hard)"
        );
        assert_eq!(constraint_summary(&[soft, hard]), "2 constraints (1 hard)");
    }

    fn window() -> ScheduleConstraint {
        ScheduleConstraint {
            time_of_day: Some(TimeOfDayRange {
                start: jiff::civil::time(9, 0, 0, 0),
                end: jiff::civil::time(17, 0, 0, 0),
            }),
            days_of_week: WeekdaySet::new(),
            date_range: None,
            severity: ConstraintSeverity::Soft,
        }
    }

    #[test]
    fn an_activity_phrase_summarizes_edits_rather_than_listing_them() {
        assert_eq!(activity_phrase(&ActivityKind::TaskCreated), "created");
        assert_eq!(
            activity_phrase(&ActivityKind::TaskUpdated { fields: 1 }),
            "updated 1 field"
        );
        assert_eq!(
            activity_phrase(&ActivityKind::TaskUpdated { fields: 7 }),
            "updated 7 fields"
        );
        assert_eq!(
            activity_phrase(&ActivityKind::TaskDeferred { count: 3 }),
            "deferred (#3)"
        );
    }

    #[test]
    fn a_focus_event_carries_its_duration() {
        let session = sunrise_id::EntityRef::new(sunrise_id::EntityKind::FocusSession, [1u8; 16]);
        let started = ActivityKind::FocusStarted {
            session,
            planned_ms: Some(1_500_000),
        };
        assert_eq!(activity_phrase(&started), "focus started (25:00)");
        let ended = ActivityKind::FocusEnded {
            session,
            focused_ms: 60_000,
            completed_task: true,
        };
        assert_eq!(activity_phrase(&ended), "focus ended (01:00, completed)");
    }

    #[test]
    fn relative_day_counts_whole_civil_days_not_elapsed_hours() {
        let tz = jiff::tz::TimeZone::UTC;
        // 23:30 today; a 00:30 tomorrow is one hour away and still "tomorrow".
        let now: jiff::Timestamp = "2026-03-02T23:30:00Z".parse().expect("ts");
        let now_ms = u64::try_from(now.as_millisecond()).expect("positive");
        let soon: jiff::Timestamp = "2026-03-03T00:30:00Z".parse().expect("ts");
        assert_eq!(relative_day(soon, now_ms, &tz), ("tomorrow".into(), false));
        assert_eq!(relative_day(now, now_ms, &tz), ("today".into(), false));
        let past: jiff::Timestamp = "2026-03-01T23:00:00Z".parse().expect("ts");
        assert_eq!(relative_day(past, now_ms, &tz), ("yesterday".into(), true));
    }

    #[test]
    fn relative_day_widens_to_a_day_count_past_the_named_days() {
        let tz = jiff::tz::TimeZone::UTC;
        let now: jiff::Timestamp = "2026-03-02T12:00:00Z".parse().expect("ts");
        let now_ms = u64::try_from(now.as_millisecond()).expect("positive");
        let ahead: jiff::Timestamp = "2026-03-05T12:00:00Z".parse().expect("ts");
        assert_eq!(relative_day(ahead, now_ms, &tz), ("+3d".into(), false));
        let behind: jiff::Timestamp = "2026-02-27T12:00:00Z".parse().expect("ts");
        assert_eq!(relative_day(behind, now_ms, &tz), ("-3d".into(), true));
    }

    #[test]
    fn the_labels_cover_every_variant() {
        assert_eq!(energy_budget_label(None), "any");
        assert_eq!(energy_budget_label(Some(Energy::High)), "high");
        assert_eq!(length_label(SessionLength::UntilDone), "until done");
        assert_eq!(energy_fit_label(EnergyFit::Over), "over");
    }
}

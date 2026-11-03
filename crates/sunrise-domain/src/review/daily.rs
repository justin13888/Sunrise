//! The optional daily review: a selection over lists the caller already read.

use super::{ts_to_ms, ReviewWindow};
use crate::task::Task;
use serde::Serialize;
use std::collections::BTreeSet;
use sunrise_id::EntityRef;

/// The optional 60-second daily flow (spec §Daily review, off by default).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DailyReview {
    /// The glance window — typically yesterday evening to now.
    pub window: ReviewWindow,
    /// Captures made in the window that are still sitting untriaged.
    pub inbox: Vec<Task>,
    /// The day's planned tasks.
    pub today: Vec<Task>,
    /// The subset of `today` that cannot be started because a blocker is still
    /// open — the concrete half of the spec's "Any blocker for today?" prompt.
    pub blocked: Vec<Task>,
}

/// Build the daily review from already-fetched lists.
///
/// `inbox` and `today` come from the existing Inbox / Today read paths;
/// `blocked_ids` is the derived-blocked set the dependency index already
/// computes. Nothing is re-derived here — the flow is a *selection*, and
/// keeping it one is what makes it a 60-second screen.
#[must_use]
pub fn build_daily_review(
    window: ReviewWindow,
    inbox: Vec<Task>,
    today: Vec<Task>,
    blocked_ids: &BTreeSet<EntityRef>,
) -> DailyReview {
    let mut recent: Vec<Task> = inbox
        .into_iter()
        .filter(|t| ts_to_ms(t.created_at).is_some_and(|ms| window.contains(ms)))
        .collect();
    recent.sort_by_key(|t| (t.created_at, t.id));

    let blocked: Vec<Task> = today
        .iter()
        .filter(|t| blocked_ids.contains(&t.id))
        .cloned()
        .collect();

    DailyReview {
        window,
        inbox: recent,
        today,
        blocked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::weekly::tests::{eref, task, DAY, MON};
    use sunrise_id::EntityKind;

    #[test]
    fn daily_review_shows_recent_captures_and_the_blocked_subset_of_today() {
        let inbox = crate::inbox::inbox_stream_ref();
        let s = eref(EntityKind::Stream, 3);
        let last_night = task(30, inbox, "captured at 22:04", MON + 20 * 3_600_000);
        let old = task(31, inbox, "captured last week", MON - 3 * DAY);
        let ready = task(32, s, "write it up", MON);
        let waiting = task(33, s, "send for review", MON);

        let daily = build_daily_review(
            ReviewWindow::new(MON + 18 * 3_600_000, MON + 30 * 3_600_000),
            vec![last_night, old],
            vec![ready, waiting.clone()],
            &BTreeSet::from([waiting.id]),
        );
        assert_eq!(
            daily
                .inbox
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            vec!["captured at 22:04"],
            "only captures inside the glance window"
        );
        assert_eq!(daily.today.len(), 2);
        assert_eq!(
            daily
                .blocked
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            vec!["send for review"]
        );
    }
}

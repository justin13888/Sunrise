//! What an OS Focus filter lets through, as arithmetic on stream ids
//! (`docs/07-clients/mobile-ios.md` §Focus Filters).
//!
//! While a Focus that carries Sunrise's filter is on, Today shows only the
//! picked streams and reminders from the other streams are muted. Every rule
//! about *which* rows that means lives here, so Today and the reminder
//! schedule cannot disagree about it, and a second client with a Focus-like
//! mechanism gets the same answers.
//!
//! A scope of `None` is "no filter". It is never the empty set: a Focus whose
//! filter names no stream filters nothing, because the other reading — an
//! empty Today and a silent phone — is a state nobody picks on purpose and
//! nothing on screen would explain.
//!
//! The scope itself is device-local and never synced; it is stored in the
//! client settings file ([`crate::preferences::ClientSettings::focus_filter`]).

use std::collections::{BTreeMap, BTreeSet};

use sunrise_domain::{ReminderIntent, Task};
use sunrise_id::EntityRef;

/// The scope a picked set of streams means: `None` for no pick or an empty
/// one.
#[must_use]
pub fn scope(picked: impl IntoIterator<Item = EntityRef>) -> Option<BTreeSet<EntityRef>> {
    let set: BTreeSet<EntityRef> = picked.into_iter().collect();
    (!set.is_empty()).then_some(set)
}

/// Whether a row filed in `stream` is shown, or its reminder fires.
///
/// An unknown stream is let through: a reminder whose entity could not be
/// read would otherwise be dropped over a failed read, and a reminder that
/// silently never fires is the worse mistake.
#[must_use]
pub fn allows(stream: Option<EntityRef>, scope: Option<&BTreeSet<EntityRef>>) -> bool {
    match (scope, stream) {
        (Some(scope), Some(stream)) => scope.contains(&stream),
        _ => true,
    }
}

/// Today's tasks, narrowed to the scope, in the order they came.
#[must_use]
pub fn scoping(tasks: Vec<Task>, scope: Option<&BTreeSet<EntityRef>>) -> Vec<Task> {
    if scope.is_none() {
        return tasks;
    }
    tasks
        .into_iter()
        .filter(|t| allows(Some(t.stream_id), scope))
        .collect()
}

/// The reminders the scope leaves audible, in the order they came.
///
/// `streams` maps a reminder's entity to the stream it is filed in; an entity
/// missing from it is let through, by [`allows`].
#[must_use]
pub fn muting(
    intents: Vec<ReminderIntent>,
    scope: Option<&BTreeSet<EntityRef>>,
    streams: &BTreeMap<EntityRef, EntityRef>,
) -> Vec<ReminderIntent> {
    if scope.is_none() {
        return intents;
    }
    intents
        .into_iter()
        .filter(|i| allows(streams.get(&i.entity).copied(), scope))
        .collect()
}

/// The line a Focus filter is described by: `All streams` with none picked,
/// up to three names joined, or the first two and how many more.
#[must_use]
pub fn summary(names: &[String]) -> String {
    match names.len() {
        0 => "All streams".to_owned(),
        1..=3 => names.join(", "),
        n => format!("{} and {} more", names[..2].join(", "), n - 2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_domain::ReminderKind;
    use sunrise_id::EntityKind;

    fn stream(n: u8) -> EntityRef {
        EntityRef::new(EntityKind::Stream, [n; 16])
    }
    fn task_ref(n: u8) -> EntityRef {
        EntityRef::new(EntityKind::Task, [n; 16])
    }

    fn intent(entity: EntityRef, title: &str) -> ReminderIntent {
        ReminderIntent {
            entity,
            kind: ReminderKind::Task,
            fire_at: jiff::Timestamp::UNIX_EPOCH,
            title: title.to_owned(),
            deferred_from: None,
        }
    }

    /// No pick, or an empty one, is no filter.
    #[test]
    fn an_empty_pick_filters_nothing() {
        assert_eq!(scope([]), None);
        assert_eq!(
            scope([stream(1), stream(1)]),
            Some(BTreeSet::from([stream(1)]))
        );
    }

    /// A row with no stream, or one whose stream could not be read, is let
    /// through: muting it would drop an alert over a failed read.
    #[test]
    fn an_unknown_stream_is_let_through() {
        let work = scope([stream(1)]);
        assert!(allows(Some(stream(1)), work.as_ref()));
        assert!(!allows(Some(stream(2)), work.as_ref()));
        assert!(allows(None, work.as_ref()));
        assert!(allows(Some(stream(2)), None));
    }

    #[test]
    fn reminders_outside_the_scope_are_muted() {
        let reminders = vec![
            intent(task_ref(1), "Work"),
            intent(task_ref(2), "Home"),
            intent(task_ref(3), "Unread"),
        ];
        let streams = BTreeMap::from([(task_ref(1), stream(1)), (task_ref(2), stream(2))]);
        let work = scope([stream(1)]);
        let kept: Vec<_> = muting(reminders.clone(), work.as_ref(), &streams)
            .into_iter()
            .map(|i| i.title)
            .collect();
        assert_eq!(kept, ["Work", "Unread"]);
        assert_eq!(muting(reminders.clone(), None, &BTreeMap::new()), reminders);
    }

    #[test]
    fn the_summary_names_up_to_three_streams() {
        let names = |n: usize| (1..=n).map(|i| format!("S{i}")).collect::<Vec<_>>();
        assert_eq!(summary(&[]), "All streams");
        assert_eq!(summary(&names(1)), "S1");
        assert_eq!(summary(&names(3)), "S1, S2, S3");
        assert_eq!(summary(&names(5)), "S1, S2 and 3 more");
    }
}

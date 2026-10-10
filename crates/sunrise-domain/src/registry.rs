//! The entity registry's records, checked against the types that carry them.
//!
//! [`sunrise_id::for_each_entity!`] declares every field of every record an
//! op writes. This module expands it into one exhaustive destructuring per
//! record, with no `..` rest pattern and every binding ascribed the registered
//! type, so:
//!
//! - a field added to a record type and not to the registry,
//! - a field registered and not on the type, and
//! - a field whose registered type is not its Rust type
//!
//! each fail this crate's build. The wire names (`as "…"` in the registry)
//! are checked by the tests below against what serde actually writes.

// Only the record types and the types their fields are written in are used,
// and only in type position.
#[allow(unused_imports)]
use crate::{
    Attachment, Block, CborValue, Chunk, Context, Energy, FocusEnd, FocusKind, FocusStart,
    Interruption, InterruptionReason, Note, NoteBody, Person, Preferences, RRule, ReviewSnapshot,
    ReviewSnapshotStream, ReviewTotals, Routine, RoutineCatchupPolicy, ScheduleConstraint,
    StreakRow, Stream, StreamColor, StreamReviewCadence, SunriseTime, Task, TaskState,
    TaskTemplate, Unknowns,
};
#[allow(unused_imports)]
use jiff::Timestamp;
#[allow(unused_imports)]
use std::collections::{BTreeMap, BTreeSet};
#[allow(unused_imports)]
use sunrise_id::EntityRef;

macro_rules! check_records {
    (
        $(
            $(#[$kind_meta:meta])*
            $kind:ident {
                prefix: $prefix:literal,
                tag: $tag:literal,
                merge: $merge:ident,
                owner: $owner:ident $(($owner_field:literal))?,
                features: [$($feature:literal),* $(,)?],
                ops: [
                    $(
                        $(#[$op_meta:meta])*
                        $op:ident($payload:ty) = $inner_kind:literal, $class:ident, $target:ident;
                    )*
                ],
                records: [
                    $(
                        $record:ident @ $storage:tt {
                            $(
                                $field:ident $(as $wire:literal)?: $field_ty:ty => $crdt:ident;
                            )*
                            $(..$unknown:ident)?
                        }
                    )*
                ],
            }
        )*
    ) => {
        $($(
            const _: () = {
                #[allow(dead_code, clippy::no_effect_underscore_binding)]
                fn registered_fields_are_the_record_fields(record: &$record) {
                    let $record { $($field,)* $($unknown,)? } = record;
                    $( let _: &$field_ty = $field; )*
                    $( let _: &Unknowns = $unknown; )?
                }
            };
        )*)*
    };
}

sunrise_id::for_each_entity!(check_records);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Frequency;
    use ciborium::Value as CborValue;
    use std::collections::BTreeSet;
    use sunrise_id::registry::{RecordSpec, ENTITIES};
    use sunrise_id::EntityKind;

    fn eref(kind: EntityKind, n: u8) -> EntityRef {
        EntityRef::new(kind, [n; 16])
    }

    fn ts() -> Timestamp {
        Timestamp::from_millisecond(1_700_000_000_000).unwrap()
    }

    fn record(name: &str) -> &'static RecordSpec {
        ENTITIES
            .iter()
            .flat_map(|e| e.records.iter())
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("{name} is not registered"))
    }

    /// The map keys serde writes for `value`, which must be the registered
    /// wire names except the ones `skipped` names: fields left at a value
    /// their `skip_serializing_if` omits.
    fn assert_wire_names<T: serde::Serialize>(name: &str, value: &T, skipped: &[&str]) {
        let mut buf = Vec::new();
        ciborium::ser::into_writer(value, &mut buf).unwrap();
        let CborValue::Map(entries) = ciborium::de::from_reader(buf.as_slice()).unwrap() else {
            panic!("{name} must encode as a map");
        };
        let written: BTreeSet<String> = entries
            .into_iter()
            .map(|(k, _)| k.into_text().expect("text key"))
            .collect();
        let registered: BTreeSet<String> = record(name)
            .fields
            .iter()
            .map(|f| f.name.to_owned())
            .collect();
        let expected: BTreeSet<String> = registered
            .iter()
            .filter(|n| !skipped.contains(&n.as_str()))
            .cloned()
            .collect();
        assert_eq!(
            written, expected,
            "{name}'s wire names drifted from the registry"
        );
        for s in skipped {
            assert!(registered.contains(*s), "{name}: `{s}` is not registered");
        }
    }

    fn template() -> TaskTemplate {
        TaskTemplate {
            title: "t".into(),
            stream_id: eref(EntityKind::Stream, 2),
            contexts: Vec::new(),
            energy: None,
            priority: None,
            estimated_duration_s: None,
            body: None,
            unknown: Unknowns::new(),
        }
    }

    #[test]
    fn lww_records_write_their_registered_names() {
        assert_wire_names(
            "Task",
            &Task {
                id: eref(EntityKind::Task, 1),
                created_at: ts(),
                updated_at: ts(),
                title: "t".into(),
                body: None,
                stream_id: eref(EntityKind::Stream, 2),
                contexts: BTreeSet::new(),
                state: TaskState::Todo,
                priority: None,
                energy: None,
                estimated_duration_s: None,
                scheduled_at: None,
                due_at: None,
                scheduling_constraints: Vec::new(),
                completed_at: None,
                deferred_count: 0,
                blocks: BTreeSet::new(),
                blocked_by: BTreeSet::new(),
                assignee: None,
                routine_id: None,
                routine_occurrence: None,
                reminder_lead_s: None,
                archived: false,
                deleted: false,
                unknown: Unknowns::new(),
            },
            &["scheduling_constraints", "reminder_lead_s"],
        );
        assert_wire_names(
            "Stream",
            &Stream {
                id: eref(EntityKind::Stream, 1),
                created_at: ts(),
                updated_at: ts(),
                name: "s".into(),
                description: None,
                color: StreamColor::Slate,
                icon: None,
                parent_id: None,
                sort_order: String::new(),
                archived: false,
                paused: false,
                paused_until: None,
                review_cadence: StreamReviewCadence::Weekly,
                default_context: None,
                reminder_lead_s: None,
                deleted: false,
                unknown: Unknowns::new(),
            },
            &["icon", "reminder_lead_s"],
        );
        assert_wire_names(
            "Context",
            &Context {
                id: eref(EntityKind::Context, 1),
                created_at: ts(),
                updated_at: ts(),
                name: "c".into(),
                description: None,
                archived: false,
                deleted: false,
                unknown: Unknowns::new(),
            },
            &[],
        );
        assert_wire_names(
            "Routine",
            &Routine {
                id: eref(EntityKind::Routine, 1),
                created_at: ts(),
                updated_at: ts(),
                template: template(),
                rrule: RRule {
                    freq: Frequency::Daily,
                    interval: 1,
                    by_day: Vec::new(),
                    by_month_day: Vec::new(),
                    by_month: Vec::new(),
                    by_set_pos: Vec::new(),
                    count: None,
                    until: None,
                    wkst: None,
                    unknown: Unknowns::new(),
                },
                timezone: "UTC".into(),
                starts_at: ts(),
                ends_at: None,
                scheduling_constraints: Vec::new(),
                skip_dates: Vec::new(),
                skipped_keys: Vec::new(),
                catchup_policy: RoutineCatchupPolicy::Skip,
                streak_counter: 0,
                last_completed_at: None,
                grace_window_s: None,
                forgiveness_enabled: true,
                streak_started_at: None,
                forgivenesses_in_window: 0,
                streak_keys: Vec::new(),
                paused: false,
                paused_until: None,
                archived: false,
                deleted: false,
                unknown: Unknowns::new(),
            },
            &[
                "scheduling_constraints",
                "skipped_keys",
                "grace_window_s",
                "forgiveness_enabled",
                "streak_started_at",
                "forgivenesses_in_window",
                "streak_keys",
            ],
        );
        assert_wire_names("TaskTemplate", &template(), &[]);
        assert_wire_names(
            "Block",
            &Block {
                id: eref(EntityKind::Block, 1),
                created_at: ts(),
                updated_at: ts(),
                stream_id: eref(EntityKind::Stream, 2),
                starts_at: SunriseTime::Instant { at: ts() },
                ends_at: SunriseTime::Instant { at: ts() },
                title: None,
                title_track_task: false,
                tasks: BTreeSet::new(),
                deleted: false,
                unknown: Unknowns::new(),
            },
            &[],
        );
        assert_wire_names(
            "Attachment",
            &Attachment {
                id: eref(EntityKind::Attachment, 1),
                created_at: ts(),
                updated_at: ts(),
                parent: eref(EntityKind::Task, 2),
                filename: "f".into(),
                mime_type: "text/plain".into(),
                size_bytes: 1,
                blob_key: [0; 32],
                blob_id: [0; 16],
                chunk_count: 1,
                content_hash: [0; 32],
                ciphertext_hash: [0; 32],
                deleted: false,
                unknown: Unknowns::new(),
            },
            &[],
        );
    }

    #[test]
    fn unsynced_records_write_their_registered_names() {
        assert_wire_names(
            "Note",
            &Note {
                id: eref(EntityKind::Note, 1),
                created_at: ts(),
                updated_at: ts(),
                parent: eref(EntityKind::Task, 2),
                body: NoteBody(Vec::new()),
                deleted: false,
                unknown: Unknowns::new(),
            },
            &[],
        );
        assert_wire_names(
            "Person",
            &Person {
                id: eref(EntityKind::Person, 1),
                created_at: ts(),
                updated_at: ts(),
                display_name: "p".into(),
                identity_id: None,
                deleted: false,
                unknown: Unknowns::new(),
            },
            &[],
        );
    }

    #[test]
    fn append_only_records_write_their_registered_names() {
        let session = eref(EntityKind::FocusSession, 1);
        assert_wire_names(
            "FocusStart",
            &FocusStart {
                id: session,
                task_id: eref(EntityKind::Task, 2),
                stream_id: eref(EntityKind::Stream, 3),
                started_at: ts(),
                planned_ms: None,
                energy: None,
                kind: FocusKind::Work,
                chunk: None,
                unknown: Unknowns::new(),
            },
            &[],
        );
        assert_wire_names(
            "FocusEnd",
            &FocusEnd {
                session_id: session,
                ended_at: ts(),
                actual_focused_ms: 0,
                interruptions: Vec::new(),
                completed_task: false,
                unknown: Unknowns::new(),
            },
            &["interruptions"],
        );
        assert_wire_names(
            "Interruption",
            &Interruption {
                session_id: session,
                at: ts(),
                reason: InterruptionReason::Meeting,
            },
            &[],
        );
        assert_wire_names(
            "ReviewSnapshot",
            &ReviewSnapshot {
                id: eref(EntityKind::ReviewSnapshot, 1),
                created_at: ts(),
                window_start: ts(),
                window_end: ts(),
                totals: ReviewTotals {
                    completed: 0,
                    deferred: 0,
                    dropped: 0,
                    created: 0,
                    reopened: 0,
                },
                streams: Vec::new(),
                streaks: Vec::new(),
                note: None,
                unknown: Unknowns::new(),
            },
            &[],
        );
    }

    #[test]
    fn preferences_write_their_registered_names() {
        assert_wire_names(
            "Preferences",
            &Preferences {
                id: crate::preferences_ref(),
                created_at: ts(),
                updated_at: ts(),
                values: BTreeMap::new(),
                unknown: Unknowns::new(),
            },
            &[],
        );
    }

    /// Every registered record is reached by one of the tests above.
    #[test]
    fn every_registered_record_is_checked_on_the_wire() {
        let checked = [
            "Task",
            "Stream",
            "Context",
            "Routine",
            "TaskTemplate",
            "Block",
            "Attachment",
            "Note",
            "Person",
            "FocusStart",
            "FocusEnd",
            "Interruption",
            "ReviewSnapshot",
            "Preferences",
        ];
        for r in ENTITIES.iter().flat_map(|e| e.records.iter()) {
            assert!(
                checked.contains(&r.name),
                "{} has no wire-name check here",
                r.name
            );
        }
    }
}

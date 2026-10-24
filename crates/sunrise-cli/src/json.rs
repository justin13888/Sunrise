//! The `--json` shapes: what a script reads instead of the table.
//!
//! Built field by field rather than by serialising the domain types, because
//! the domain's serde names are the domain's to change and these are a
//! contract: a field here is added, never renamed or removed, and every name
//! is documented in `docs/07-clients/overview.md` §JSON output. Times carry
//! their kind, exactly as the table does, so a script can tell a floating
//! 09:00 from a pinned one (`docs/11-adr/0066-times-survive-travel.md` §4).

use jiff::tz::TimeZone;
use serde_json::{json, Value};
use sunrise_core::queries::{ContextRow, DeviceRow, StreamRow};
use sunrise_domain::{Block, RoutineRow, SunriseTime, Task};
use sunrise_id::EntityRef;

/// An id as text.
fn id(r: EntityRef) -> Value {
    Value::String(r.to_str())
}

fn ids<'a>(refs: impl IntoIterator<Item = &'a EntityRef>) -> Value {
    Value::Array(refs.into_iter().copied().map(id).collect())
}

/// A stored time: `{kind, value, zone, at_ms}`.
///
/// `kind` is `instant`, `zoned`, `floating`, `all_day`, or a newer build's
/// kind verbatim; `value` is the round-trippable rendering; `zone` is a zoned
/// value's own zone; `at_ms` is where it falls for this reader, `null` when
/// it cannot be placed.
#[must_use]
pub fn time(v: &SunriseTime, reader: &TimeZone) -> Value {
    let (kind, zone) = match v {
        SunriseTime::Instant { .. } => ("instant".to_owned(), None),
        SunriseTime::Zoned { tz, .. } => ("zoned".to_owned(), Some(tz.clone())),
        SunriseTime::Floating { .. } => ("floating".to_owned(), None),
        SunriseTime::AllDay { .. } => ("all_day".to_owned(), None),
        SunriseTime::Unknown { kind, .. } => (kind.clone(), None),
    };
    json!({
        "kind": kind,
        "value": v.to_string(),
        "zone": zone,
        "at_ms": v.resolve_in(reader).map(jiff::Timestamp::as_millisecond),
    })
}

fn opt_time(v: Option<&SunriseTime>, reader: &TimeZone) -> Value {
    v.map_or(Value::Null, |v| time(v, reader))
}

/// A task: every field a triage pass reads. `note` is the note as plain
/// text (Markdown), `null` when there is none.
#[must_use]
pub fn task(t: &Task, reader: &TimeZone) -> Value {
    let note = t.body.as_ref().filter(|b| !b.is_empty()).map(|b| {
        sunrise_domain::note_body_to_markdown(&sunrise_domain::decode_note_body(b).blocks)
    });
    json!({
        "id": t.id.to_str(),
        "title": t.title,
        "state": t.state.as_str(),
        "stream": t.stream_id.to_str(),
        "contexts": ids(&t.contexts),
        "priority": t.priority,
        "energy": t.energy.as_ref().map(sunrise_domain::Energy::as_str),
        "estimate_s": t.estimated_duration_s,
        "scheduled_at": opt_time(t.scheduled_at.as_ref(), reader),
        "due_at": opt_time(t.due_at.as_ref(), reader),
        "completed_at": opt_time(t.completed_at.as_ref(), reader),
        "deferred_count": t.deferred_count,
        "blocked_by": ids(&t.blocked_by),
        "note": note,
        "updated_at_ms": t.updated_at.as_millisecond(),
    })
}

/// A list of tasks.
#[must_use]
pub fn tasks(ts: &[Task], reader: &TimeZone) -> Value {
    Value::Array(ts.iter().map(|t| task(t, reader)).collect())
}

/// A row of `sunrise streams`.
#[must_use]
pub fn stream(s: &StreamRow) -> Value {
    json!({
        "id": s.id.to_str(),
        "name": s.name,
        "color": s.color.as_str(),
        "open_tasks": s.open_task_count,
        "archived": s.archived,
        "paused": s.paused,
        "sort_order": s.sort_order,
    })
}

/// A row of `sunrise contexts`.
#[must_use]
pub fn context(c: &ContextRow) -> Value {
    json!({
        "id": c.id.to_str(),
        "name": c.name,
        "description": c.description,
        "tasks": c.task_count,
        "archived": c.archived,
    })
}

/// A row of `sunrise routines`.
#[must_use]
pub fn routine(r: &RoutineRow) -> Value {
    json!({
        "id": r.id.to_str(),
        "title": r.title,
        "rule": r.rrule,
        "next_ms": r.next.map(jiff::Timestamp::as_millisecond),
        "paused": r.paused,
        "streak": r.streak,
    })
}

/// A row of `sunrise devices`.
#[must_use]
pub fn device(d: &DeviceRow, this_device: &[u8; 16]) -> Value {
    json!({
        "id": sunrise_client_core::ids::hex_id(&d.device_id),
        "nickname": d.nickname,
        "platform": d.platform,
        "this_device": &d.device_id == this_device,
        "revoked": d.revoked,
        "read_bounded": d.read_bounded,
        "current": d.current,
        "admitted_after_revocation": d.admitted_after_revocation,
    })
}

/// A block on the grid. `title` is the one shown, after the shadow-copy and
/// title-tracking rules.
#[must_use]
pub fn block(b: &Block, title: Option<&str>, reader: &TimeZone) -> Value {
    json!({
        "id": b.id.to_str(),
        "title": title,
        "stream": b.stream_id.to_str(),
        "starts_at": time(&b.starts_at, reader),
        "ends_at": time(&b.ends_at, reader),
        "tasks": ids(&b.tasks),
    })
}

/// The Upcoming view: `{today, span_days, days, no_date}`.
///
/// Each day is `{date, start_ms, end_ms, blocks, planned, due}`, its bounds
/// in the reader's zone (23 or 25 hours apart on a DST day); each no-date
/// group is `{stream, no_stream, tasks, total}`.
#[must_use]
pub fn upcoming(u: &sunrise_core::queries::Upcoming, reader: &TimeZone) -> Value {
    let days: Vec<Value> = u
        .days
        .iter()
        .map(|d| {
            json!({
                "date": d.date.to_string(),
                "start_ms": d.start.as_millisecond(),
                "end_ms": d.end.as_millisecond(),
                "blocks": d
                    .blocks
                    .iter()
                    .map(|r| block(&r.block, r.title.as_deref(), reader))
                    .collect::<Vec<_>>(),
                "planned": tasks(&d.planned, reader),
                "due": tasks(&d.due, reader),
            })
        })
        .collect();
    let no_date: Vec<Value> = u
        .no_date
        .iter()
        .map(|g| {
            json!({
                "stream": g.stream.to_str(),
                "no_stream": g.no_stream,
                "tasks": tasks(&g.tasks, reader),
                "total": g.total,
            })
        })
        .collect();
    json!({
        "today": u.today.to_string(),
        "span_days": u.span_days,
        "days": days,
        "no_date": no_date,
    })
}

/// Print `v` as one line on stdout: the whole of a `--json` command's output.
#[allow(clippy::print_stdout)]
pub fn print(v: &Value) {
    println!("{v}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ny() -> TimeZone {
        TimeZone::get("America/New_York").unwrap()
    }

    #[test]
    fn a_time_carries_its_kind_and_where_it_falls_for_this_reader() {
        let civil = jiff::civil::date(2026, 10, 24).at(9, 0, 0, 0);
        let floating = time(&SunriseTime::floating(civil), &ny());
        assert_eq!(floating["kind"], "floating");
        assert_eq!(floating["value"], "2026-10-24T09:00:00");
        assert_eq!(floating["zone"], Value::Null);
        // 09:00 in New York (EDT) is 13:00Z.
        let at: jiff::Timestamp = "2026-10-24T13:00:00Z".parse().unwrap();
        assert_eq!(floating["at_ms"], at.as_millisecond());

        let zoned = time(&SunriseTime::zoned(civil, "Europe/Berlin"), &ny());
        assert_eq!(zoned["kind"], "zoned");
        assert_eq!(zoned["zone"], "Europe/Berlin");
    }

    #[test]
    fn a_task_has_the_documented_fields() {
        let t = Task {
            title: "Renew passport".into(),
            priority: Some(1),
            ..sunrise_test_task()
        };
        let v = task(&t, &ny());
        for key in [
            "id",
            "title",
            "state",
            "stream",
            "contexts",
            "priority",
            "energy",
            "estimate_s",
            "scheduled_at",
            "due_at",
            "completed_at",
            "deferred_count",
            "blocked_by",
            "note",
            "updated_at_ms",
        ] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
        assert_eq!(v["title"], "Renew passport");
        assert_eq!(v["priority"], 1);
        assert_eq!(v["scheduled_at"], Value::Null);
    }

    /// A task with every field at its empty value.
    fn sunrise_test_task() -> Task {
        let at = jiff::Timestamp::UNIX_EPOCH;
        Task {
            id: EntityRef::new(sunrise_id::EntityKind::Task, [1; 16]),
            created_at: at,
            updated_at: at,
            title: String::new(),
            body: None,
            stream_id: sunrise_domain::inbox_stream_ref(),
            contexts: std::collections::BTreeSet::new(),
            state: sunrise_domain::TaskState::Todo,
            priority: None,
            energy: None,
            estimated_duration_s: None,
            scheduled_at: None,
            due_at: None,
            scheduling_constraints: Vec::new(),
            completed_at: None,
            deferred_count: 0,
            blocks: std::collections::BTreeSet::new(),
            blocked_by: std::collections::BTreeSet::new(),
            assignee: None,
            routine_id: None,
            routine_occurrence: None,
            reminder_lead_s: None,
            archived: false,
            deleted: false,
            unknown: sunrise_domain::Unknowns::new(),
        }
    }
}

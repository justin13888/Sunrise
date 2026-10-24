//! The Upcoming view across the seam (#351): [`sunrise_core::queries::Upcoming`],
//! mirrored. Every grouping, ordering and day boundary is the core's; a client
//! renders these rows in the order they arrive.

use jiff::civil::Date as CivilDate;
use sunrise_core::queries::{Upcoming, UpcomingDay};
use sunrise_domain::NoDateGroup;
use sunrise_id::EntityRef;

use crate::dto::{BlockGridRow, TaskItem};

/// One civil day of Upcoming, in the device's zone.
#[derive(Debug, Clone, uniffi::Record)]
pub struct UpcomingDayRow {
    /// The civil date, `YYYY-MM-DD`.
    pub date: CivilDate,
    /// When the day starts: civil midnight in the device's zone.
    pub start: jiff::Timestamp,
    /// When the next day starts. 23 or 25 hours after `start` on a DST day.
    pub end: jiff::Timestamp,
    /// Blocks overlapping the day, by start. One that runs past midnight is
    /// on both days.
    pub blocks: Vec<BlockGridRow>,
    /// Planned for the day: timed ones in time order, then all-day ones by
    /// stream.
    pub planned: Vec<TaskItem>,
    /// Due that day and not planned for it, by deadline, then priority.
    pub due: Vec<TaskItem>,
}

impl From<&UpcomingDay> for UpcomingDayRow {
    fn from(d: &UpcomingDay) -> Self {
        Self {
            date: d.date,
            start: d.start,
            end: d.end,
            blocks: d.blocks.iter().map(BlockGridRow::from).collect(),
            planned: d.planned.iter().map(TaskItem::from).collect(),
            due: d.due.iter().map(TaskItem::from).collect(),
        }
    }
}

/// One stream's group in Upcoming's no-date lane.
#[derive(Debug, Clone, uniffi::Record)]
pub struct NoDateLaneGroup {
    /// The stream; the Inbox when `no_stream` is set.
    pub stream: EntityRef,
    /// Whether this is the "No stream" group.
    pub no_stream: bool,
    /// The first five tasks, by deadline, then priority.
    pub tasks: Vec<TaskItem>,
    /// How many the group holds; `total` minus the rows shown is "N more".
    pub total: u32,
}

impl From<&NoDateGroup> for NoDateLaneGroup {
    fn from(g: &NoDateGroup) -> Self {
        Self {
            stream: g.stream,
            no_stream: g.no_stream,
            tasks: g.tasks.iter().map(TaskItem::from).collect(),
            total: g.total,
        }
    }
}

/// The Upcoming view: one row per civil day from today, then the no-date
/// lane.
#[derive(Debug, Clone, uniffi::Record)]
pub struct UpcomingReport {
    /// The device's civil today, `YYYY-MM-DD`.
    pub today: CivilDate,
    /// 7, 14 or 30.
    pub span_days: u32,
    /// `span_days` days, from today.
    pub days: Vec<UpcomingDayRow>,
    /// Open undated tasks by stream, in the stream list's order, "No stream"
    /// last.
    pub no_date: Vec<NoDateLaneGroup>,
}

impl From<&Upcoming> for UpcomingReport {
    fn from(u: &Upcoming) -> Self {
        Self {
            today: u.today,
            span_days: u.span_days,
            days: u.days.iter().map(UpcomingDayRow::from).collect(),
            no_date: u.no_date.iter().map(NoDateLaneGroup::from).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{CoreQuery, CoreQueryResult};
    use sunrise_core::{Query, QueryResult};
    use sunrise_id::EntityKind;

    #[test]
    fn upcoming_lowers_to_the_core_query_with_its_span() {
        let ctx = EntityRef::new(EntityKind::Context, [3; 16]);
        let q = CoreQuery::Upcoming {
            now_ms: 42,
            span_days: Some(14),
            contexts: vec![ctx],
        }
        .into_core();
        assert!(matches!(
            q,
            Query::Upcoming { now_ms: 42, span_days: Some(14), ref contexts } if contexts == &[ctx]
        ));
    }

    #[test]
    fn upcoming_lifts_with_every_day_and_group() {
        let tz = jiff::tz::TimeZone::get("America/New_York").unwrap();
        let today = jiff::civil::date(2026, 3, 5);
        let view = sunrise_domain::build_upcoming(sunrise_domain::UpcomingInput {
            tasks: &[],
            blocks: &[],
            block_of: |r: &sunrise_core::queries::BlockRow| &r.block,
            stream_order: &[],
            no_stream: sunrise_domain::inbox_stream_ref(),
            contexts: &[],
            today,
            span_days: 7,
            tz: &tz,
            schedule: &sunrise_domain::DaySchedule::default(),
        })
        .unwrap();
        let CoreQueryResult::Upcoming { upcoming } =
            CoreQueryResult::from_core(QueryResult::Upcoming(Box::new(view)))
        else {
            panic!("Upcoming lifted into another variant");
        };
        assert_eq!(upcoming.today, today);
        assert_eq!(upcoming.span_days, 7);
        assert_eq!(upcoming.days.len(), 7);
        assert_eq!(
            upcoming.days[3].end.as_millisecond() - upcoming.days[3].start.as_millisecond(),
            23 * 3_600_000,
            "2026-03-08 is 23 hours in New York"
        );
        assert!(upcoming.no_date.is_empty());
    }
}

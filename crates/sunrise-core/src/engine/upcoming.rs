//! `Query::Upcoming` (`docs/08-features/planning-views.md` §Upcoming, #351):
//! read the rows, resolve the civil bounds, and hand both to the pure
//! `sunrise_domain::build_upcoming`.

use super::ids::{blob16, ms_to_ts};
use super::task::read_task;
use super::{Engine, EngineError};
use crate::queries::{BlockRow, QueryResult};
use sunrise_domain::time::PREFILTER_SLACK_MS;
use sunrise_domain::{
    build_upcoming, inbox_stream_ref, is_upcoming_span, upcoming_days, PrefValue, UpcomingError,
    UpcomingInput, DEFAULT_UPCOMING_SPAN_DAYS,
};
use sunrise_id::EntityRef;
use sunrise_storage::Db;

/// The device-local preference the span falls back to.
const SPAN_KEY: &str = "views.upcoming.span_days";

impl Engine {
    /// The Upcoming view from the civil day containing `now_ms` in the
    /// device's zone.
    ///
    /// The SQL reads every open task with no `scheduled_at` (the no-date
    /// lane) and every one whose `scheduled_at` or `due_at` index falls in
    /// the span widened by [`PREFILTER_SLACK_MS`]; which day a task is on is
    /// then decided on its resolved value (`docs/10-cross-cutting/time.md`
    /// §2 rule 2). Blocks come from the calendar's own overlap read.
    pub(super) fn query_upcoming(
        &self,
        db: &Db,
        now_ms: u64,
        span_days: Option<u32>,
        contexts: &[EntityRef],
    ) -> Result<QueryResult, EngineError> {
        let span = match span_days {
            Some(n) => n,
            None => self.upcoming_span(db)?,
        };
        let tz = self.device_zone();
        let today = ms_to_ts(i64::try_from(now_ms).unwrap_or(i64::MAX))
            .to_zoned(tz.clone())
            .date();
        let days = upcoming_days(today, span, &tz).map_err(upcoming_error)?;
        let (Some(first), Some(last)) = (days.first(), days.last()) else {
            return Err(upcoming_error(UpcomingError::Span(span)));
        };
        let (from, to) = (first.1.as_millisecond(), last.2.as_millisecond());

        let tasks = read_upcoming_tasks(
            db,
            from.saturating_sub(PREFILTER_SLACK_MS),
            to.saturating_add(PREFILTER_SLACK_MS),
        )?;
        let QueryResult::Blocks(blocks) = self.query_blocks_between(db, from, to)? else {
            return Err(EngineError::Invalid(
                "block read returned non-blocks".into(),
            ));
        };
        let QueryResult::Streams(streams) = self.query_stream_list(db)? else {
            return Err(EngineError::Invalid(
                "stream read returned non-streams".into(),
            ));
        };
        let stream_order: Vec<EntityRef> = streams.iter().map(|s| s.id).collect();

        let view = build_upcoming(UpcomingInput {
            tasks: &tasks,
            blocks: &blocks,
            block_of: |row: &BlockRow| &row.block,
            stream_order: &stream_order,
            no_stream: inbox_stream_ref(),
            contexts,
            today,
            span_days: span,
            tz: &tz,
        })
        .map_err(upcoming_error)?;
        Ok(QueryResult::Upcoming(Box::new(view)))
    }

    /// The `views.upcoming.span_days` preference, or the default when it is
    /// unset or holds a span the view does not offer.
    fn upcoming_span(&self, db: &Db) -> Result<u32, EngineError> {
        Ok(match self.resolved_preference(db, SPAN_KEY)? {
            Some(PrefValue::Uint(n)) => u32::try_from(n)
                .ok()
                .filter(|n| is_upcoming_span(*n))
                .unwrap_or(DEFAULT_UPCOMING_SPAN_DAYS),
            _ => DEFAULT_UPCOMING_SPAN_DAYS,
        })
    }
}

fn upcoming_error(e: UpcomingError) -> EngineError {
    EngineError::Invalid(e.to_string())
}

/// Open, unarchived tasks that are undated, or whose plan or deadline index
/// falls in `[from, to)`.
fn read_upcoming_tasks(
    db: &Db,
    from: i64,
    to: i64,
) -> Result<Vec<sunrise_domain::Task>, EngineError> {
    let mut stmt = db.conn().prepare(
        "SELECT id FROM tasks
         WHERE deleted = 0 AND archived = 0
           AND state != 'done' AND state != 'cancelled'
           AND (scheduled_at_ms IS NULL
             OR (scheduled_at_ms >= ?1 AND scheduled_at_ms < ?2)
             OR (due_at_ms IS NOT NULL AND due_at_ms >= ?1 AND due_at_ms < ?2))
         ORDER BY id ASC",
    )?;
    let ids = stmt
        .query_map(rusqlite::params![from, to], |r| r.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::with_capacity(ids.len());
    for raw in ids {
        if let Some(t) = read_task(db.conn(), &blob16(&raw))? {
            out.push(t);
        }
    }
    Ok(out)
}

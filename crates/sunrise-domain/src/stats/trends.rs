//! Completed-per-week, deferred-per-week and created-per-week trends.
//!
//! The spec pins the one definition that is easy to get wrong:
//!
//! > **Completed-per-week** = transitions to `done` whose `done_at` falls in
//! > the week. Re-opens that flip back to `pending` decrement the count for the
//! > week of the **original** completion, so the chart is stable.
//!
//! That is why the trend is folded from the **op log** and not from
//! `tasks.completed_at`: the materialized column is a last-writer-wins
//! register, so re-opening a task erases the very instant the chart needs to
//! decrement. The log keeps both facts, in order, forever.

use super::WeekGrid;
use crate::activity::{OpPayload, OpRecord};
use crate::task::TaskState;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use sunrise_id::EntityRef;

/// Trend length the spec asks for: the last 12 weeks.
pub const TREND_WEEKS: u32 = 12;

/// One week of one trend line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeekBucket {
    /// Start of the week (ms since epoch, in the grid's zone).
    pub week_start_ms: u64,
    /// Net transitions to `done` credited to this week. Signed: a re-open
    /// decrements the week of the original completion, so a bucket can go
    /// negative if a task completed before the window is re-opened inside it.
    pub completed: i64,
    /// Defers recorded in this week.
    pub deferred: i64,
    /// Tasks created in this week.
    pub created: i64,
}

impl WeekBucket {
    /// An empty bucket for `week_start_ms`.
    #[must_use]
    pub const fn empty(week_start_ms: u64) -> Self {
        Self {
            week_start_ms,
            completed: 0,
            deferred: 0,
            created: 0,
        }
    }
}

/// One Stream's trend line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamTrend {
    /// Stream the line belongs to.
    pub stream: EntityRef,
    /// One bucket per grid week, ascending.
    pub weeks: Vec<WeekBucket>,
}

/// Completed / deferred / created trends, whole-vault and per Stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trends {
    /// Bucket starts, ascending — the x axis.
    pub week_starts: Vec<u64>,
    /// Whole-vault line.
    pub overall: Vec<WeekBucket>,
    /// Per-Stream lines, ordered by stream id.
    pub per_stream: Vec<StreamTrend>,
}

impl Trends {
    /// The line for one Stream, if it has any activity in the window.
    #[must_use]
    pub fn for_stream(&self, stream: EntityRef) -> Option<&[WeekBucket]> {
        self.per_stream
            .iter()
            .find(|t| t.stream == stream)
            .map(|t| t.weeks.as_slice())
    }
}

/// What we remember about a Task between ops while folding.
#[derive(Debug, Clone)]
struct TaskTrace {
    state: TaskState,
    deferred_count: i64,
    /// The instant and Stream a completion was *counted at*. Kept so a later
    /// re-open decrements the same bucket it incremented, which is the whole
    /// stability property the spec asks for.
    counted: Option<(u64, EntityRef)>,
}

/// Apply `f` to bucket `idx` of both the whole-vault line and `stream`'s line,
/// creating the Stream's line (zero-filled to the grid width) on first use.
fn bump(
    grid: &WeekGrid,
    overall: &mut [WeekBucket],
    per_stream: &mut BTreeMap<EntityRef, Vec<WeekBucket>>,
    stream: EntityRef,
    idx: usize,
    f: impl Fn(&mut WeekBucket),
) {
    f(&mut overall[idx]);
    let line = per_stream.entry(stream).or_insert_with(|| {
        grid.starts()
            .iter()
            .map(|s| WeekBucket::empty(*s))
            .collect()
    });
    f(&mut line[idx]);
}

/// Fold task ops into weekly trends.
///
/// `ops` must be in ascending `(at_ms, op_id)` order and should carry each
/// task's **full** history — a task whose create is outside the window still
/// needs its earlier state to tell a transition from a no-op write.
///
/// A completion is credited to the week of `completed_at` when the op set one,
/// falling back to the op's own timestamp; that is the spec's `done_at`.
#[must_use]
pub fn fold_trends(ops: &[OpRecord], grid: &WeekGrid) -> Trends {
    let mut overall: Vec<WeekBucket> = grid
        .starts()
        .iter()
        .map(|s| WeekBucket::empty(*s))
        .collect();
    let mut per_stream: BTreeMap<EntityRef, Vec<WeekBucket>> = BTreeMap::new();
    let mut trace: BTreeMap<EntityRef, TaskTrace> = BTreeMap::new();

    for op in ops {
        let (task, created) = match &op.payload {
            OpPayload::TaskCreated(t) => (t, true),
            OpPayload::TaskUpdated(t) => (t, false),
            _ => continue,
        };
        let prev = trace.get(&task.id).cloned();
        if created {
            if let Some(i) = grid.index_of(op.at_ms) {
                bump(
                    grid,
                    &mut overall,
                    &mut per_stream,
                    crate::stream_key(task.stream_id),
                    i,
                    |b| {
                        b.created += 1;
                    },
                );
            }
        }

        let was_done = prev.as_ref().is_some_and(|p| p.state == TaskState::Done);
        let is_done = task.state == TaskState::Done;
        let mut counted = prev.as_ref().and_then(|p| p.counted);

        // Baseline rule: the first row we see for a task establishes state.
        // Only a *create* may also be a completion (a task created done); an
        // update with no predecessor is not evidence of a transition.
        let transitioned_to_done = is_done && !was_done && (prev.is_some() || created);
        let transitioned_from_done = was_done && !is_done;

        if transitioned_to_done {
            let done_at = task
                .completed_at
                .as_ref()
                .and_then(|t| t.resolve_in(grid.zone()))
                .and_then(|t| u64::try_from(t.as_millisecond()).ok())
                .unwrap_or(op.at_ms);
            if let Some(i) = grid.index_of(done_at) {
                bump(
                    grid,
                    &mut overall,
                    &mut per_stream,
                    crate::stream_key(task.stream_id),
                    i,
                    |b| {
                        b.completed += 1;
                    },
                );
            }
            counted = Some((done_at, crate::stream_key(task.stream_id)));
        } else if transitioned_from_done {
            if let Some((at, stream)) = counted {
                if let Some(i) = grid.index_of(at) {
                    bump(grid, &mut overall, &mut per_stream, stream, i, |b| {
                        b.completed -= 1;
                    });
                }
            }
            counted = None;
        }

        if let Some(p) = &prev {
            let delta = task.deferred_count.saturating_sub(p.deferred_count);
            if delta > 0 {
                if let Some(i) = grid.index_of(op.at_ms) {
                    bump(
                        grid,
                        &mut overall,
                        &mut per_stream,
                        crate::stream_key(task.stream_id),
                        i,
                        |b| {
                            b.deferred += delta;
                        },
                    );
                }
            }
        }

        trace.insert(
            task.id,
            TaskTrace {
                state: task.state.clone(),
                deferred_count: task.deferred_count,
                counted,
            },
        );
    }

    Trends {
        week_starts: grid.starts().to_vec(),
        overall,
        per_stream: per_stream
            .into_iter()
            .map(|(stream, weeks)| StreamTrend { stream, weeks })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{eref, grid3, ts, MON, WEEK_MS};
    use super::*;
    use crate::task::Task;
    use crate::unknown::Unknowns;
    use std::collections::BTreeSet;
    use sunrise_id::EntityKind;

    fn task(id: u8, stream: u8) -> Task {
        Task {
            reminder_lead_s: None,
            id: eref(EntityKind::Task, id),
            created_at: ts(MON),
            updated_at: ts(MON),
            title: "t".into(),
            body: None,
            stream_id: Some(eref(EntityKind::Stream, stream)),
            contexts: BTreeSet::new(),
            state: TaskState::Todo,
            priority: None,
            energy: None,
            estimated_duration_s: None,
            planned_at: None,
            target_at: None,
            hard_due_at: None,
            scheduling_constraints: Vec::new(),
            completed_at: None,
            deferred_count: 0,
            blocks: BTreeSet::new(),
            blocked_by: BTreeSet::new(),
            assignee: None,
            routine_id: None,
            routine_occurrence: None,
            hard_constraint_violated: false,
            sort_order: None,
            triaged_at: None,
            cancel_reason: None,
            late_acknowledged_at: None,
            archived: false,
            deleted: false,
            unknown: Unknowns::new(),
        }
    }

    fn op(n: u8, at_ms: u64, payload: OpPayload) -> OpRecord {
        let target = match &payload {
            OpPayload::TaskCreated(t) | OpPayload::TaskUpdated(t) => t.id,
            _ => eref(EntityKind::Task, 0),
        };
        OpRecord {
            op_id: [n; 16],
            at_ms,
            device: [1; 16],
            target,
            payload,
        }
    }

    #[test]
    fn completion_is_credited_to_the_week_of_done_at_not_of_the_op() {
        let g = grid3();
        let t = task(1, 9);
        let mut done = t.clone();
        done.state = TaskState::Done;
        // Completed in week 1, but the op that carries it is stamped in week 2
        // (a device that was offline and synced late).
        done.completed_at = Some(ts(MON - WEEK_MS + 3_600_000).into());

        let trends = fold_trends(
            &[
                op(1, MON - WEEK_MS, OpPayload::TaskCreated(Box::new(t))),
                op(2, MON + 1_000, OpPayload::TaskUpdated(Box::new(done))),
            ],
            &g,
        );
        let completed: Vec<i64> = trends.overall.iter().map(|b| b.completed).collect();
        assert_eq!(completed, vec![0, 1, 0], "credited to done_at's week");
    }

    #[test]
    fn a_reopen_decrements_the_week_of_the_original_completion() {
        let g = grid3();
        let t = task(1, 9);
        let mut done = t.clone();
        done.state = TaskState::Done;
        done.completed_at = Some(ts(MON - WEEK_MS).into());
        let mut reopened = done.clone();
        reopened.state = TaskState::Todo;
        reopened.completed_at = None;

        let trends = fold_trends(
            &[
                op(1, MON - 2 * WEEK_MS, OpPayload::TaskCreated(Box::new(t))),
                op(2, MON - WEEK_MS, OpPayload::TaskUpdated(Box::new(done))),
                // Re-opened two weeks later, in the current week.
                op(3, MON + 1_000, OpPayload::TaskUpdated(Box::new(reopened))),
            ],
            &g,
        );
        let completed: Vec<i64> = trends.overall.iter().map(|b| b.completed).collect();
        assert_eq!(
            completed,
            vec![0, 0, 0],
            "week 1's +1 is cancelled where it was counted, not where the re-open landed"
        );
    }

    #[test]
    fn completing_reopening_and_recompleting_nets_out_to_the_new_week() {
        let g = grid3();
        let t = task(1, 9);
        let mut done = t.clone();
        done.state = TaskState::Done;
        done.completed_at = Some(ts(MON - 2 * WEEK_MS).into());
        let mut reopened = done.clone();
        reopened.state = TaskState::Todo;
        reopened.completed_at = None;
        let mut redone = reopened.clone();
        redone.state = TaskState::Done;
        redone.completed_at = Some(ts(MON).into());

        let trends = fold_trends(
            &[
                op(1, MON - 2 * WEEK_MS, OpPayload::TaskCreated(Box::new(t))),
                op(2, MON - 2 * WEEK_MS, OpPayload::TaskUpdated(Box::new(done))),
                op(3, MON - WEEK_MS, OpPayload::TaskUpdated(Box::new(reopened))),
                op(4, MON, OpPayload::TaskUpdated(Box::new(redone))),
            ],
            &g,
        );
        let completed: Vec<i64> = trends.overall.iter().map(|b| b.completed).collect();
        assert_eq!(completed, vec![0, 0, 1], "one task, counted exactly once");
    }

    #[test]
    fn a_reopen_of_a_completion_older_than_the_grid_has_nowhere_to_land() {
        let g = grid3();
        let t = task(1, 9);
        let mut done = t.clone();
        done.state = TaskState::Done;
        done.completed_at = Some(ts(MON - 40 * WEEK_MS).into());
        let mut reopened = done.clone();
        reopened.state = TaskState::Todo;

        let trends = fold_trends(
            &[
                op(1, MON - 40 * WEEK_MS, OpPayload::TaskCreated(Box::new(t))),
                op(
                    2,
                    MON - 40 * WEEK_MS,
                    OpPayload::TaskUpdated(Box::new(done)),
                ),
                op(3, MON, OpPayload::TaskUpdated(Box::new(reopened))),
            ],
            &g,
        );
        assert!(
            trends.overall.iter().all(|b| b.completed == 0),
            "an out-of-window decrement is dropped, not folded into week 0: {:?}",
            trends.overall
        );
    }

    #[test]
    fn defers_are_counted_per_increment_and_split_per_stream() {
        let g = grid3();
        let a = task(1, 7);
        let b = task(2, 8);
        let mut a1 = a.clone();
        a1.deferred_count = 1;
        let mut a2 = a1.clone();
        a2.deferred_count = 3; // two defers batched into one op
        let mut b1 = b.clone();
        b1.deferred_count = 1;

        let trends = fold_trends(
            &[
                op(1, MON - WEEK_MS, OpPayload::TaskCreated(Box::new(a))),
                op(2, MON - WEEK_MS, OpPayload::TaskUpdated(Box::new(a1))),
                op(3, MON, OpPayload::TaskUpdated(Box::new(a2))),
                op(4, MON, OpPayload::TaskCreated(Box::new(b))),
                op(5, MON, OpPayload::TaskUpdated(Box::new(b1))),
            ],
            &g,
        );
        let deferred: Vec<i64> = trends.overall.iter().map(|b| b.deferred).collect();
        assert_eq!(deferred, vec![0, 1, 3], "2 from `a2`, 1 from `b1`");

        let line_a = trends.for_stream(eref(EntityKind::Stream, 7)).unwrap();
        let line_b = trends.for_stream(eref(EntityKind::Stream, 8)).unwrap();
        assert!(
            trends.for_stream(eref(EntityKind::Stream, 99)).is_none(),
            "a Stream with no activity in the window has no line at all — not \
             a zero-filled one, which a chart would draw"
        );
        assert_eq!(
            line_a.iter().map(|b| b.deferred).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(
            line_b.iter().map(|b| b.deferred).collect::<Vec<_>>(),
            vec![0, 0, 1]
        );
    }

    #[test]
    fn creates_are_bucketed_and_a_task_created_done_counts_once() {
        let g = grid3();
        let mut t = task(1, 9);
        t.state = TaskState::Done;
        t.completed_at = Some(ts(MON).into());
        let trends = fold_trends(&[op(1, MON, OpPayload::TaskCreated(Box::new(t)))], &g);
        assert_eq!(trends.overall[2].created, 1);
        assert_eq!(trends.overall[2].completed, 1);
    }

    #[test]
    fn an_update_with_no_predecessor_is_not_a_completion() {
        let g = grid3();
        let mut done = task(1, 9);
        done.state = TaskState::Done;
        done.completed_at = Some(ts(MON).into());
        // Only the update is in the input — the create was compacted away.
        let trends = fold_trends(&[op(1, MON, OpPayload::TaskUpdated(Box::new(done)))], &g);
        assert!(
            trends.overall.iter().all(|b| b.completed == 0),
            "no baseline means no transition: {:?}",
            trends.overall
        );
    }

    #[test]
    fn per_stream_lines_all_have_the_grid_width() {
        let g = grid3();
        let t = task(1, 9);
        let trends = fold_trends(&[op(1, MON, OpPayload::TaskCreated(Box::new(t)))], &g);
        assert_eq!(trends.week_starts.len(), 3);
        for line in &trends.per_stream {
            assert_eq!(line.weeks.len(), 3);
            assert_eq!(
                line.weeks
                    .iter()
                    .map(|b| b.week_start_ms)
                    .collect::<Vec<_>>(),
                trends.week_starts
            );
        }
    }
}

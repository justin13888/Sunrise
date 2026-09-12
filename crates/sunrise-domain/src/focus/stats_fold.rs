//! The estimate calibration fold: the immutable session log reduced to
//! totals, per-Stream and per-energy breakdowns, and the estimate-vs-actual
//! factors behind "your 30-minute estimates run ~1.7x long".
//!
//! The only part of [`crate::focus`] that accumulates. Every bucket is a
//! [`BTreeMap`] or a [`BTreeSet`], so the output ordering is the keys' own and
//! never the order the records happened to arrive in — which is what makes a
//! fold over a replicated log deterministic. It is also the only part that
//! takes a `now_ms`, and that clock reaches exactly one number,
//! `total_focused_ms`, never a calibration factor.

use super::planner::energy_rank;
use super::session::{FocusKind, Interruption, InterruptionReason};
use crate::common::Energy;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use sunrise_id::EntityRef;

// ---------------------------------------------------------------------------
// Estimate calibration — a pure fold over the immutable session log
// ---------------------------------------------------------------------------

/// One row of the session log as the fold sees it: the session's own facts
/// joined with the *task's* estimate.
///
/// Deliberately flat and owned so [`fold_focus_stats`] needs no database, no
/// graph, and no clock beyond the `now_ms` the caller passes in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// Session id.
    pub session: EntityRef,
    /// Task focused on.
    pub task: EntityRef,
    /// Owning Stream — the per-Stream calibration bucket.
    pub stream: EntityRef,
    /// The session's declared energy budget — the per-energy bucket.
    pub energy: Option<Energy>,
    /// Work or break.
    pub kind: FocusKind,
    /// Start, ms since epoch.
    pub started_at_ms: u64,
    /// End, ms since epoch; `None` while the session runs.
    pub ended_at_ms: Option<u64>,
    /// Frozen focused time; `None` while the session runs.
    pub actual_focused_ms: Option<u64>,
    /// The task's `estimated_duration_s` at read time.
    pub estimated_duration_s: Option<u64>,
    /// Whether the task was completed in this session.
    pub completed_task: bool,
    /// Interruptions logged against this session.
    pub interruptions: Vec<Interruption>,
}

impl SessionRecord {
    /// Focused time contributed by this record: frozen once ended, derived
    /// from the clock while running.
    #[must_use]
    pub fn focused_ms(&self, now_ms: u64) -> u64 {
        match self.actual_focused_ms {
            Some(ms) => ms,
            None => now_ms.saturating_sub(self.started_at_ms),
        }
    }

    /// A session still running has no `end` op yet.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.ended_at_ms.is_none()
    }
}

/// The estimate-vs-actual calibration factor for one bucket.
///
/// `factor > 1.0` means work runs **long** against its estimate — "your
/// 30-minute estimates run ~1.7× long" is `factor == 1.7`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    /// `actual_ms / estimated_ms`.
    pub factor: f64,
    /// Completed, estimated tasks behind the factor.
    pub samples: u32,
    /// Total estimated time across those tasks.
    pub estimated_ms: u64,
    /// Total recorded focused time across those tasks.
    pub actual_ms: u64,
}

/// Focus totals for one Stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamFocus {
    /// Stream id.
    pub stream: EntityRef,
    /// Work sessions recorded against it.
    pub sessions: u32,
    /// Total focused time.
    pub focused_ms: u64,
    /// Calibration for this Stream, when it has at least one completed,
    /// estimated task.
    pub calibration: Option<Calibration>,
}

/// Focus totals for one energy budget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnergyFocus {
    /// The declared budget the sessions ran under; `None` groups sessions that
    /// declared none.
    pub energy: Option<Energy>,
    /// Work sessions recorded under it.
    pub sessions: u32,
    /// Total focused time.
    pub focused_ms: u64,
    /// Calibration for this energy budget.
    pub calibration: Option<Calibration>,
}

/// How often one interruption reason came up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptionTally {
    /// The reason.
    pub reason: InterruptionReason,
    /// Occurrences.
    pub count: u32,
}

/// The whole focus picture: totals, per-Stream and per-energy breakdowns, the
/// calibration factors, and the top focus-breakers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FocusStats {
    /// Every session in range, work and break.
    pub sessions: u32,
    /// Work sessions only.
    pub work_sessions: u32,
    /// Sessions with a `start` and no `end` — still running.
    pub running: u32,
    /// Total focused (work) time, with running sessions counted from the
    /// caller's clock.
    pub total_focused_ms: u64,
    /// Total interruptions logged.
    pub interruptions: u32,
    /// Per-Stream breakdown, ordered by stream id.
    pub per_stream: Vec<StreamFocus>,
    /// Per-energy breakdown, ordered Low, Med, High, then unset.
    pub per_energy: Vec<EnergyFocus>,
    /// Whole-vault calibration.
    pub overall: Option<Calibration>,
    /// Top focus-breakers, most frequent first.
    pub top_interruptions: Vec<InterruptionTally>,
}

/// Accumulator for one calibration bucket: estimated and actual, per task.
#[derive(Debug, Default)]
struct CalibAcc {
    /// task → (estimate_ms, actual_ms, completed)
    tasks: BTreeMap<EntityRef, (u64, u64, bool)>,
}

impl CalibAcc {
    fn observe(&mut self, r: &SessionRecord) {
        // Running sessions are excluded: their focused time is derived from
        // the clock, and a factor that drifts with wall time is not a
        // calibration. Only frozen `actual_focused_ms` feeds the fold.
        let Some(actual) = r.actual_focused_ms else {
            return;
        };
        let est_ms = r.estimated_duration_s.unwrap_or(0).saturating_mul(1000);
        let e = self.tasks.entry(r.task).or_insert((0, 0, false));
        e.0 = est_ms;
        e.1 = e.1.saturating_add(actual);
        e.2 |= r.completed_task;
    }

    /// Only **completed** tasks with a usable estimate calibrate: a task still
    /// in flight has an actual that is a lower bound, not a measurement.
    #[allow(clippy::cast_precision_loss)]
    fn finish(&self) -> Option<Calibration> {
        let mut estimated_ms: u64 = 0;
        let mut actual_ms: u64 = 0;
        let mut samples: u32 = 0;
        for (est, act, completed) in self.tasks.values() {
            if !*completed || *est == 0 {
                continue;
            }
            estimated_ms = estimated_ms.saturating_add(*est);
            actual_ms = actual_ms.saturating_add(*act);
            samples = samples.saturating_add(1);
        }
        if samples == 0 || estimated_ms == 0 {
            return None;
        }
        Some(Calibration {
            factor: actual_ms as f64 / estimated_ms as f64,
            samples,
            estimated_ms,
            actual_ms,
        })
    }
}

/// Fold the immutable session log into [`FocusStats`].
///
/// Pure: same input, same output, with `now_ms` the only time that enters —
/// and it only affects the *running* sessions' contribution to
/// `total_focused_ms`, never a calibration factor. A running session has no
/// frozen `actual_focused_ms`, and a factor derived from a duration that grows
/// with the wall clock would not be a calibration, so those sessions are left
/// out of the calibration fold entirely.
///
/// Breaks are counted in `sessions` but contribute no focused time and no
/// calibration: a break is not work.
#[must_use]
pub fn fold_focus_stats(records: &[SessionRecord], now_ms: u64) -> FocusStats {
    let mut sessions: u32 = 0;
    let mut work_sessions: u32 = 0;
    let mut running: u32 = 0;
    let mut total_focused_ms: u64 = 0;
    let mut interruptions: u32 = 0;

    let mut stream_sessions: BTreeMap<EntityRef, (u32, u64)> = BTreeMap::new();
    let mut energy_sessions: BTreeMap<Option<Energy>, (u32, u64)> = BTreeMap::new();
    let mut stream_calib: BTreeMap<EntityRef, CalibAcc> = BTreeMap::new();
    let mut energy_calib: BTreeMap<Option<Energy>, CalibAcc> = BTreeMap::new();
    let mut overall = CalibAcc::default();
    let mut reasons: BTreeMap<InterruptionReason, u32> = BTreeMap::new();
    // Interruptions are set-semantics: the same triple may reach us both as
    // its own op and inside an `end` op's list.
    let mut seen_interruptions: BTreeSet<Interruption> = BTreeSet::new();

    for r in records {
        sessions = sessions.saturating_add(1);
        if r.is_running() {
            running = running.saturating_add(1);
        }
        for i in &r.interruptions {
            if seen_interruptions.insert(*i) {
                interruptions = interruptions.saturating_add(1);
                *reasons.entry(i.reason).or_insert(0) += 1;
            }
        }
        if r.kind != FocusKind::Work {
            continue;
        }
        work_sessions = work_sessions.saturating_add(1);
        let focused = r.focused_ms(now_ms);
        total_focused_ms = total_focused_ms.saturating_add(focused);

        let s = stream_sessions.entry(r.stream).or_insert((0, 0));
        s.0 = s.0.saturating_add(1);
        s.1 = s.1.saturating_add(focused);

        let e = energy_sessions.entry(r.energy).or_insert((0, 0));
        e.0 = e.0.saturating_add(1);
        e.1 = e.1.saturating_add(focused);

        overall.observe(r);
        stream_calib.entry(r.stream).or_default().observe(r);
        energy_calib.entry(r.energy).or_default().observe(r);
    }

    let per_stream = stream_sessions
        .into_iter()
        .map(|(stream, (n, ms))| StreamFocus {
            stream,
            sessions: n,
            focused_ms: ms,
            calibration: stream_calib.get(&stream).and_then(CalibAcc::finish),
        })
        .collect();

    let mut per_energy: Vec<EnergyFocus> = energy_sessions
        .into_iter()
        .map(|(energy, (n, ms))| EnergyFocus {
            energy,
            sessions: n,
            focused_ms: ms,
            calibration: energy_calib.get(&energy).and_then(CalibAcc::finish),
        })
        .collect();
    // `Option<Energy>` sorts None first; Low → Med → High → unset reads better.
    per_energy.sort_by_key(|e| e.energy.map_or(u8::MAX, energy_rank));

    let mut top_interruptions: Vec<InterruptionTally> = reasons
        .into_iter()
        .map(|(reason, count)| InterruptionTally { reason, count })
        .collect();
    top_interruptions.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.reason.cmp(&b.reason)));

    FocusStats {
        sessions,
        work_sessions,
        running,
        total_focused_ms,
        interruptions,
        per_stream,
        per_energy,
        overall: overall.finish(),
        top_interruptions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_id::EntityKind;

    fn fcs(b: u8) -> EntityRef {
        EntityRef::new(EntityKind::FocusSession, [b; 16])
    }
    fn tsk(b: u8) -> EntityRef {
        EntityRef::new(EntityKind::Task, [b; 16])
    }
    fn strm(b: u8) -> EntityRef {
        EntityRef::new(EntityKind::Stream, [b; 16])
    }

    fn record(
        id: u8,
        task: u8,
        est_s: Option<u64>,
        actual: Option<u64>,
        done: bool,
    ) -> SessionRecord {
        SessionRecord {
            session: fcs(id),
            task: tsk(task),
            stream: strm(1),
            energy: Some(Energy::High),
            kind: FocusKind::Work,
            started_at_ms: 1_000,
            ended_at_ms: actual.map(|a| 1_000 + a),
            actual_focused_ms: actual,
            estimated_duration_s: est_s,
            completed_task: done,
            interruptions: Vec::new(),
        }
    }

    // ---- calibration ----

    #[test]
    fn calibration_produces_the_factor_from_known_pairs() {
        // "Your 30-minute estimates run ~1.7x long": estimate 30 min, actual
        // 51 min, task completed.
        let recs = vec![record(1, 1, Some(30 * 60), Some(51 * 60 * 1000), true)];
        let s = fold_focus_stats(&recs, 0);
        let c = s.overall.expect("one completed, estimated task calibrates");
        assert!((c.factor - 1.7).abs() < 1e-9, "factor was {}", c.factor);
        assert_eq!(c.samples, 1);
        assert_eq!(c.estimated_ms, 30 * 60 * 1000);
        assert_eq!(c.actual_ms, 51 * 60 * 1000);
    }

    #[test]
    fn calibration_aggregates_sessions_of_one_task_before_dividing() {
        // A 60-minute estimate finished across three 30-minute sittings is a
        // 1.5x factor, not three separate ones.
        let recs = vec![
            record(1, 1, Some(60 * 60), Some(30 * 60 * 1000), false),
            record(2, 1, Some(60 * 60), Some(30 * 60 * 1000), false),
            record(3, 1, Some(60 * 60), Some(30 * 60 * 1000), true),
        ];
        let c = fold_focus_stats(&recs, 0).overall.expect("calibrates");
        assert!((c.factor - 1.5).abs() < 1e-9, "factor was {}", c.factor);
        assert_eq!(c.samples, 1, "one task, not three");
    }

    #[test]
    fn calibration_ignores_incomplete_and_unestimated_work() {
        let recs = vec![
            // Estimated but never finished: its actual is a lower bound.
            record(1, 1, Some(600), Some(1_200_000), false),
            // Finished but never estimated: nothing to calibrate against.
            record(2, 2, None, Some(600_000), true),
        ];
        let s = fold_focus_stats(&recs, 0);
        assert_eq!(s.overall, None);
        // The time is still counted, it just does not calibrate.
        assert_eq!(s.total_focused_ms, 1_800_000);
        assert_eq!(s.work_sessions, 2);
    }

    #[test]
    fn a_running_session_counts_time_but_never_calibrates() {
        let mut running = record(1, 1, Some(600), None, false);
        running.started_at_ms = 1_000;
        let done = record(2, 2, Some(600), Some(900_000), true);
        let s = fold_focus_stats(&[running, done], 1_000 + 300_000);
        assert_eq!(s.running, 1);
        // 300 s derived from the clock + 900 s frozen.
        assert_eq!(s.total_focused_ms, 300_000 + 900_000);
        // The factor comes from the frozen session alone: 900 s actual against
        // a 600 s estimate.
        let c = s.overall.expect("calibrates");
        assert!((c.factor - 1.5).abs() < 1e-9);
        assert_eq!(c.samples, 1);
    }

    #[test]
    fn breaks_are_recorded_but_never_counted_as_focus() {
        let mut brk = record(2, 1, Some(600), Some(300_000), false);
        brk.kind = FocusKind::Break;
        let s = fold_focus_stats(&[record(1, 1, Some(600), Some(600_000), true), brk], 0);
        assert_eq!(s.sessions, 2);
        assert_eq!(s.work_sessions, 1);
        assert_eq!(s.total_focused_ms, 600_000);
        let c = s.overall.expect("calibrates");
        assert!(
            (c.factor - 1.0).abs() < 1e-9,
            "the break did not inflate it"
        );
    }

    #[test]
    fn stats_bucket_per_stream_and_per_energy() {
        let mut a = record(1, 1, Some(600), Some(1_200_000), true); // 2.0x
        a.stream = strm(1);
        a.energy = Some(Energy::High);
        let mut b = record(2, 2, Some(600), Some(300_000), true); // 0.5x
        b.stream = strm(2);
        b.energy = Some(Energy::Low);
        let s = fold_focus_stats(&[a, b], 0);
        assert_eq!(s.per_stream.len(), 2);
        let s1 = s.per_stream.iter().find(|r| r.stream == strm(1)).unwrap();
        assert!((s1.calibration.unwrap().factor - 2.0).abs() < 1e-9);
        let s2 = s.per_stream.iter().find(|r| r.stream == strm(2)).unwrap();
        assert!((s2.calibration.unwrap().factor - 0.5).abs() < 1e-9);
        // Low before High, per the ordering contract.
        assert_eq!(
            s.per_energy.iter().map(|e| e.energy).collect::<Vec<_>>(),
            vec![Some(Energy::Low), Some(Energy::High)]
        );
        // Overall pools both: 1500 s actual against 1200 s estimated.
        let c = s.overall.unwrap();
        assert_eq!(c.samples, 2);
        assert!((c.factor - 1.25).abs() < 1e-9);
    }

    #[test]
    fn interruptions_dedupe_and_rank() {
        let dup = Interruption {
            session_id: fcs(1),
            at: crate::epoch_ms::from_u64(5_000),
            reason: InterruptionReason::Meeting,
        };
        let mut r1 = record(1, 1, None, Some(600_000), true);
        // The same triple reaching us twice — once as its own op, once inside
        // the `end` op — is one interruption.
        r1.interruptions = vec![
            dup,
            dup,
            Interruption {
                session_id: fcs(1),
                at: crate::epoch_ms::from_u64(9_000),
                reason: InterruptionReason::SelfInterrupt,
            },
        ];
        let mut r2 = record(2, 2, None, Some(600_000), true);
        r2.interruptions = vec![Interruption {
            session_id: fcs(2),
            at: crate::epoch_ms::from_u64(1_000),
            reason: InterruptionReason::Meeting,
        }];
        let s = fold_focus_stats(&[r1, r2], 0);
        assert_eq!(s.interruptions, 3);
        assert_eq!(
            s.top_interruptions,
            vec![
                InterruptionTally {
                    reason: InterruptionReason::Meeting,
                    count: 2
                },
                InterruptionTally {
                    reason: InterruptionReason::SelfInterrupt,
                    count: 1
                },
            ]
        );
    }

    #[test]
    fn empty_log_folds_to_zero() {
        let s = fold_focus_stats(&[], 12_345);
        assert_eq!(s.sessions, 0);
        assert_eq!(s.total_focused_ms, 0);
        assert_eq!(s.overall, None);
        assert!(s.per_stream.is_empty());
        assert!(s.top_interruptions.is_empty());
    }
}

//! Dataset builders — the four tables `docs/08-features/reviews-and-stats.md`
//! §Export makes available. Each is a plain projection of a fold's output; none
//! of them recompute anything.

use super::table::{Cell, Table};
use crate::activity::{ActivityEvent, ActivityKind};
use crate::focus::FocusStats;
use crate::stats::Trends;
use crate::streak::StreakRow;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use sunrise_id::EntityRef;

/// Which dataset an export request wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportDataset {
    /// Per-week completed / deferred / created, whole-vault and per Stream.
    Trends,
    /// The activity timeline.
    Activity,
    /// Time-in-focus and the estimate calibration, per Stream.
    Focus,
    /// Routine streaks.
    Streaks,
}

impl ExportDataset {
    /// Lowercase wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trends => "trends",
            Self::Activity => "activity",
            Self::Focus => "focus",
            Self::Streaks => "streaks",
        }
    }

    /// Parse from the lowercase wire name.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "trends" => Some(Self::Trends),
            "activity" => Some(Self::Activity),
            "focus" => Some(Self::Focus),
            "streaks" => Some(Self::Streaks),
            _ => None,
        }
    }
}

/// Long-format trend table: one row per `(week, stream)` plus the whole-vault
/// rows under the reserved stream name `"(all)"`.
///
/// Long format rather than a week-per-column pivot because the number of weeks
/// is a parameter: a pivot would give the CSV a variable header, which every
/// downstream tool has to special-case.
#[must_use]
pub fn trends_table(trends: &Trends, names: &BTreeMap<EntityRef, String>) -> Table {
    let mut t = Table::new(
        ExportDataset::Trends.as_str(),
        &[
            "week_start_ms",
            "stream_id",
            "stream_name",
            "completed",
            "deferred",
            "created",
        ],
    );
    for b in &trends.overall {
        t.push(vec![
            Cell::u64(b.week_start_ms),
            Cell::Null,
            Cell::text("(all)"),
            Cell::Int(b.completed),
            Cell::Int(b.deferred),
            Cell::Int(b.created),
        ]);
    }
    for line in &trends.per_stream {
        let name = names
            .get(&line.stream)
            .cloned()
            .unwrap_or_else(|| line.stream.to_str());
        for b in &line.weeks {
            t.push(vec![
                Cell::u64(b.week_start_ms),
                Cell::text(line.stream.to_str()),
                Cell::text(name.clone()),
                Cell::Int(b.completed),
                Cell::Int(b.deferred),
                Cell::Int(b.created),
            ]);
        }
    }
    t
}

/// Activity timeline table.
///
/// `detail` carries the one number or reference the verb needs — the field
/// count for an update, the destination for a move — so the table stays flat
/// without losing the payload.
#[must_use]
pub fn activity_table(events: &[ActivityEvent]) -> Table {
    let mut t = Table::new(
        ExportDataset::Activity.as_str(),
        &["at_ms", "entity_id", "label", "kind", "detail", "device"],
    );
    for e in events {
        let detail = match &e.kind {
            ActivityKind::TaskDeferred { count } => Cell::Int(*count),
            ActivityKind::TaskUpdated { fields } => Cell::u32(*fields),
            ActivityKind::TaskMoved { to, .. } => {
                to.map_or(Cell::Null, |to| Cell::text(to.to_str()))
            }
            ActivityKind::FocusStarted { planned_ms, .. } => Cell::opt_ms(*planned_ms),
            ActivityKind::FocusEnded { focused_ms, .. } => Cell::u64(*focused_ms),
            _ => Cell::Null,
        };
        t.push(vec![
            Cell::u64(e.at_ms),
            Cell::text(e.entity.to_str()),
            Cell::text(e.label.clone()),
            Cell::text(e.kind.verb()),
            detail,
            Cell::text(hex16(&e.device)),
        ]);
    }
    t
}

/// Per-Stream time-in-focus and estimate calibration, with a whole-vault row.
#[must_use]
pub fn focus_table(stats: &FocusStats, names: &BTreeMap<EntityRef, String>) -> Table {
    let mut t = Table::new(
        ExportDataset::Focus.as_str(),
        &[
            "stream_id",
            "stream_name",
            "sessions",
            "focused_ms",
            "calibration_factor",
            "calibration_samples",
        ],
    );
    t.push(vec![
        Cell::Null,
        Cell::text("(all)"),
        Cell::u32(stats.work_sessions),
        Cell::u64(stats.total_focused_ms),
        stats.overall.map_or(Cell::Null, |c| Cell::Float(c.factor)),
        stats.overall.map_or(Cell::Null, |c| Cell::u32(c.samples)),
    ]);
    for s in &stats.per_stream {
        let name = names
            .get(&s.stream)
            .cloned()
            .unwrap_or_else(|| s.stream.to_str());
        t.push(vec![
            Cell::text(s.stream.to_str()),
            Cell::text(name),
            Cell::u32(s.sessions),
            Cell::u64(s.focused_ms),
            s.calibration.map_or(Cell::Null, |c| Cell::Float(c.factor)),
            s.calibration.map_or(Cell::Null, |c| Cell::u32(c.samples)),
        ]);
    }
    t
}

/// Routine streak table.
#[must_use]
pub fn streaks_table(rows: &[StreakRow]) -> Table {
    let mut t = Table::new(
        ExportDataset::Streaks.as_str(),
        &["routine_id", "title", "streak", "last_completed_at_ms"],
    );
    for r in rows {
        t.push(vec![
            Cell::text(r.routine.to_str()),
            Cell::text(r.title.clone()),
            Cell::Int(r.streak),
            Cell::opt_ms(r.last_completed_at_ms),
        ]);
    }
    t
}

/// Lowercase hex of a 16-byte device id.
fn hex16(bytes: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

//! Lateness and the ADR-0047 Today across the seam (#334).
//!
//! A client renders what these carry and never decides any of it: the core
//! reads each task's lateness for the reader now, in the reader's zone, on
//! the vault's planner days (`sunrise_domain::lateness`), and groups Today
//! from it. Every enum here is new, so no shipped Swift switches over one.

use crate::dto::TaskItem;
use sunrise_core::{TaskLateness, TodayRow};
use sunrise_domain::{Lateness, LatenessState, TodayGroup, TodayRowDeadline};
use sunrise_id::EntityRef;

/// See [`sunrise_domain::Lateness`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LatenessKind {
    /// The hard deadline has been missed.
    LateHard,
    /// The soft deadline has been missed, and the hard one has not.
    LateSoft,
    /// No deadline, and untouched for `stale_after_days` planner days.
    Stale,
    /// None of the above.
    OnTrack,
}

impl From<Lateness> for LatenessKind {
    fn from(l: Lateness) -> Self {
        match l {
            Lateness::LateHard => Self::LateHard,
            Lateness::LateSoft => Self::LateSoft,
            Lateness::Stale => Self::Stale,
            Lateness::OnTrack => Self::OnTrack,
        }
    }
}

/// See [`sunrise_domain::LatenessState`]: the state, and when it began.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct LatenessItem {
    /// The state.
    pub state: LatenessKind,
    /// When it began: the missed deadline, or the start of the planner day
    /// the task turned stale on. None when on track.
    pub since: Option<jiff::Timestamp>,
}

impl From<&LatenessState> for LatenessItem {
    fn from(l: &LatenessState) -> Self {
        Self {
            state: l.state.into(),
            since: l.since,
        }
    }
}

/// See [`sunrise_domain::TodayGroup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TodayGroupItem {
    /// Planned for today.
    Planned,
    /// Planned for an earlier day and not late: carried over, never rolled.
    CarriedOver,
    /// A deadline falls today and the task is not planned for today.
    DueToday,
    /// Late, soft or hard. Folded by default.
    Late,
}

impl From<TodayGroup> for TodayGroupItem {
    fn from(g: TodayGroup) -> Self {
        match g {
            TodayGroup::Planned => Self::Planned,
            TodayGroup::CarriedOver => Self::CarriedOver,
            TodayGroup::DueToday => Self::DueToday,
            TodayGroup::Late => Self::Late,
        }
    }
}

/// See [`sunrise_domain::TodayRowDeadline`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TodayDeadlineItem {
    /// The hard deadline falls today.
    Hard,
    /// Only the soft deadline does.
    Target,
}

impl From<TodayRowDeadline> for TodayDeadlineItem {
    fn from(d: TodayRowDeadline) -> Self {
        match d {
            TodayRowDeadline::Hard => Self::Hard,
            TodayRowDeadline::Target => Self::Target,
        }
    }
}

/// One row of `CoreQuery.todayView`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct TodayRowItem {
    /// The task.
    pub task: TaskItem,
    /// Its group.
    pub group: TodayGroupItem,
    /// For `dueToday`, which deadline falls today.
    pub deadline: Option<TodayDeadlineItem>,
    /// Its lateness for the reader now.
    pub lateness: LatenessItem,
}

impl From<&TodayRow> for TodayRowItem {
    fn from(r: &TodayRow) -> Self {
        Self {
            task: TaskItem::from(&r.task),
            group: r.group.into(),
            deadline: r.deadline.map(Into::into),
            lateness: LatenessItem::from(&r.lateness),
        }
    }
}

/// One row of `CoreQuery.lateness`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct TaskLatenessItem {
    /// The task.
    pub task: EntityRef,
    /// Its lateness for the reader now.
    pub lateness: LatenessItem,
}

impl From<&TaskLateness> for TaskLatenessItem {
    fn from(r: &TaskLateness) -> Self {
        Self {
            task: r.task,
            lateness: LatenessItem::from(&r.lateness),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_and_group_lifts_to_its_own_case() {
        for l in Lateness::ALL {
            let lifted = LatenessKind::from(l);
            let back = match lifted {
                LatenessKind::LateHard => Lateness::LateHard,
                LatenessKind::LateSoft => Lateness::LateSoft,
                LatenessKind::Stale => Lateness::Stale,
                LatenessKind::OnTrack => Lateness::OnTrack,
            };
            assert_eq!(back, l);
        }
        let since = jiff::Timestamp::UNIX_EPOCH;
        let item = LatenessItem::from(&LatenessState {
            state: Lateness::Stale,
            since: Some(since),
        });
        assert_eq!(item.state, LatenessKind::Stale);
        assert_eq!(item.since, Some(since));
        assert_eq!(
            TodayGroupItem::from(TodayGroup::CarriedOver),
            TodayGroupItem::CarriedOver
        );
        assert_eq!(
            TodayGroupItem::from(TodayGroup::Planned),
            TodayGroupItem::Planned
        );
        assert_eq!(
            TodayGroupItem::from(TodayGroup::DueToday),
            TodayGroupItem::DueToday
        );
        assert_eq!(TodayGroupItem::from(TodayGroup::Late), TodayGroupItem::Late);
        assert_eq!(
            TodayDeadlineItem::from(TodayRowDeadline::Target),
            TodayDeadlineItem::Target
        );
        assert_eq!(
            TodayDeadlineItem::from(TodayRowDeadline::Hard),
            TodayDeadlineItem::Hard
        );
    }
}

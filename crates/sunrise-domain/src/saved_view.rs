//! Saved views: a named view and filter that syncs (`docs/08-features/planning-views.md`
//! §Saved views, issue #341).
//!
//! A saved view is a **spec**, an ordinary synced entity, and its **result**
//! is recomputed on each device: the core evaluates it on every read
//! (`Query::SavedView`) and never stores what it matched. The spec is
//! written only by `Patch` (ADR-0044), one register per field, so a rename on
//! one device and a filter edit on another both survive.
//!
//! # The filter
//!
//! [`ViewFilter`] is a typed predicate. Every part is optional and they are
//! ANDed; inside a part, values are ORed (any of these contexts). Contexts
//! and streams are named by **id**, so renaming a context keeps every view
//! that names it working. A context name a one-time import could not resolve
//! is kept in [`ViewFilter::context_names`] and reported, not dropped.
//!
//! A part this build does not know (a newer build's place or lateness
//! predicate) is kept verbatim in the filter's unknown fields and written
//! back with it (ADR-0045 §6, #322). It cannot be evaluated here, so a view
//! holding one evaluates without it and says so
//! ([`ViewFilter::is_complete`]): showing more than the newer build would is
//! better than silently showing less.
//!
//! # Not built yet
//!
//! * `TODO(#332)`: a "no stream" selector. Until tasks may have no stream,
//!   the Inbox is the stream that means it, and [`ViewFilter::streams`] names
//!   it like any other.
//! * `TODO(#334)`: a lateness predicate. `Task` has no lateness state yet.
//! * `TODO(#339)`: a place predicate. There is no Place entity yet.
//! * `TODO(#336)`: the date window is over `scheduled_at` and `due_at`, the
//!   two times `Task` has; ADR-0047's `planned_at`, `target_at` and
//!   `hard_due_at` arrive with #334.
//! * `TODO(#345)`: [`ViewFilter::text`] is the search grammar for a Search
//!   view, run by the core's search; for every other view it is a plain
//!   case-insensitive match on the title.
//!
//! Each arrives as a new named part of [`ViewFilter`], which an older build
//! keeps as an unknown field, so none of them needs a new entity shape.

use crate::common::Energy;
use crate::time::SunriseTime;
use crate::unknown::{UnknownVariant, Unknowns};
use crate::validation::{validate_title, ValidationError};
use crate::Task;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use sunrise_id::EntityRef;

/// The longest view name, in characters after trimming.
pub const MAX_VIEW_NAME_LEN: usize = 128;

/// The dotted field path a `SavedView.sort_order` validation failure names.
pub const SORT_ORDER_FIELD: &str = "saved_view.sort_order";

/// Which primary view a saved view opens (`docs/07-clients/parity-matrix.md`).
///
/// Lossless (#321): a view a newer build added is kept and written back, and
/// evaluates to no task list here.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ViewKind {
    /// Today.
    Today,
    /// The Inbox.
    Inbox,
    /// One or more Streams, or every task.
    Stream,
    /// A search.
    Search,
    /// The calendar grid.
    Calendar,
    /// Focus mode.
    Focus,
    /// Routines.
    Routines,
    /// The review flow.
    Review,
    /// The morning summary.
    Morning,
    /// The end-of-day brief.
    Evening,
    /// A view this build does not know, kept verbatim (ADR-0045 §6).
    Unknown(UnknownVariant),
}

crate::unknown::lossy_enum!(ViewKind, {
    Today => "today",
    Inbox => "inbox",
    Stream => "stream",
    Search => "search",
    Calendar => "calendar",
    Focus => "focus",
    Routines => "routines",
    Review => "review",
    Morning => "morning",
    Evening => "evening",
});

impl ViewKind {
    /// Whether the view is a list of tasks the filter narrows. The others
    /// open a screen of their own and have nothing to filter.
    #[must_use]
    pub const fn lists_tasks(&self) -> bool {
        matches!(
            self,
            Self::Today | Self::Inbox | Self::Stream | Self::Search
        )
    }
}

/// How a saved view's tasks are ordered.
///
/// Lossless; an order this build does not know reads as [`ViewSort::Default`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum ViewSort {
    /// The view's own order: Today by urgency, a list by manual order, a
    /// search by rank.
    #[default]
    Default,
    /// Manual order (`Task.sort_order`, #340).
    Manual,
    /// Oldest first.
    Created,
    /// Most recently changed first.
    Updated,
    /// By title.
    Title,
    /// Highest priority first; unset last.
    Priority,
    /// Soonest deadline first; none last.
    Due,
    /// Soonest plan first; none last.
    Scheduled,
    /// An order this build does not know, kept verbatim.
    Unknown(UnknownVariant),
}

crate::unknown::lossy_enum!(ViewSort, fallback = Default, {
    Default => "default",
    Manual => "manual",
    Created => "created",
    Updated => "updated",
    Title => "title",
    Priority => "priority",
    Due => "due",
    Scheduled => "scheduled",
});

/// Which of a task's times a [`DateWindow`] reads.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WindowField {
    /// `scheduled_at`, the plan.
    Scheduled,
    /// `due_at`, the deadline.
    Due,
    /// A time this build does not know, kept verbatim.
    Unknown(UnknownVariant),
}

crate::unknown::lossy_enum!(WindowField, {
    Scheduled => "scheduled",
    Due => "due",
});

/// An inclusive priority range, `1` the highest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorityRange {
    /// The highest priority kept (the smallest number).
    pub min: u8,
    /// The lowest priority kept (the largest number).
    pub max: u8,
    /// Fields a newer build added.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

/// A window of civil days relative to the reader's today, both ends
/// inclusive, over one of a task's times (`docs/10-cross-cutting/time.md`
/// §2): the day a time falls on is the day a reader in their zone sees it
/// on, so a floating 09:00 is on the same day for every reader.
///
/// `from_days: None` is open into the past, `to_days: None` open into the
/// future. `{from_days: None, to_days: 0}` is "on or before today". A task
/// with no such time is outside every window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DateWindow {
    /// The time it reads.
    pub field: WindowField,
    /// The first day, in days from today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_days: Option<i32>,
    /// The last day, in days from today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_days: Option<i32>,
    /// Fields a newer build added.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

/// What a saved view narrows its tasks to. See the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewFilter {
    /// Tasks in any of these Streams. Empty is no filter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub streams: Vec<EntityRef>,
    /// Tasks carrying any of these Contexts, by id. Empty is no filter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contexts: Vec<EntityRef>,
    /// Context names an import could not resolve to an id. Reported on every
    /// read and never matched: a name is what broke when a context was
    /// renamed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_names: Vec<String>,
    /// Tasks whose priority is in this range; an unset priority is outside.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<PriorityRange>,
    /// Tasks with any of these energies. Empty is no filter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub energy: Vec<Energy>,
    /// Tasks one of whose times falls in this window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<DateWindow>,
    /// Free text: the query of a Search view, and a title match otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Filter parts a newer build added, kept verbatim.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

/// What evaluating a filter needs that is not in the task: the reader.
#[derive(Debug, Clone)]
pub struct FilterReader {
    /// The reader's zone.
    pub tz: TimeZone,
    /// The reader's today, the planner day holding now.
    pub today: Date,
}

impl ViewFilter {
    /// Whether every part of the filter is one this build evaluates.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.unknown.is_empty()
            && self.priority.as_ref().is_none_or(|p| p.unknown.is_empty())
            && self
                .window
                .as_ref()
                .is_none_or(|w| w.unknown.is_empty() && !w.field.is_unknown())
    }

    /// Whether `task` passes every part of the filter this build knows. With
    /// `with_text: false` the text part is skipped, for a Search view whose
    /// text the search itself applied.
    #[must_use]
    pub fn matches(&self, task: &Task, reader: &FilterReader, with_text: bool) -> bool {
        if !self.streams.is_empty() && !self.streams.contains(&task.stream_id) {
            return false;
        }
        if !self.contexts.is_empty() && !self.contexts.iter().any(|c| task.contexts.contains(c)) {
            return false;
        }
        if let Some(range) = &self.priority {
            if !task
                .priority
                .is_some_and(|p| (range.min..=range.max).contains(&p))
            {
                return false;
            }
        }
        if !self.energy.is_empty()
            && !task
                .energy
                .as_ref()
                .is_some_and(|e| self.energy.contains(e))
        {
            return false;
        }
        if let Some(window) = &self.window {
            if !window.contains(task, reader) {
                return false;
            }
        }
        if with_text && !self.text.trim().is_empty() {
            let needle = self.text.trim().to_lowercase();
            if !task.title.to_lowercase().contains(&needle) {
                return false;
            }
        }
        true
    }
}

impl DateWindow {
    /// Whether `task`'s time falls in the window for `reader`. A field this
    /// build does not know passes: see [`ViewFilter::is_complete`].
    #[must_use]
    pub fn contains(&self, task: &Task, reader: &FilterReader) -> bool {
        let time: Option<&SunriseTime> = match self.field {
            WindowField::Scheduled => task.scheduled_at.as_ref(),
            WindowField::Due => task.due_at.as_ref(),
            WindowField::Unknown(_) => return true,
        };
        let Some(day) = time.and_then(|t| t.day_in(&reader.tz)) else {
            return false;
        };
        let offset = |n: i32| {
            reader
                .today
                .checked_add(jiff::Span::new().days(i64::from(n)))
        };
        let after_start = self
            .from_days
            .is_none_or(|n| offset(n).is_ok_and(|from| day >= from));
        let before_end = self
            .to_days
            .is_none_or(|n| offset(n).is_ok_and(|to| day <= to));
        after_start && before_end
    }
}

/// Order `tasks` by `sort` for a reader in `tz`. [`ViewSort::Default`] keeps
/// the order they came in; every other order ends on id, so two replicas
/// show one order.
pub fn sort_tasks(tasks: &mut [Task], sort: &ViewSort, tz: &TimeZone) {
    let by_time = |a: Option<&SunriseTime>, b: Option<&SunriseTime>| match (a, b) {
        (Some(x), Some(y)) => x.cmp_in(y, tz),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    };
    match sort.effective() {
        ViewSort::Manual => crate::task_order::sort_manually(tasks),
        ViewSort::Created => {
            tasks.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        }
        ViewSort::Updated => {
            tasks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));
        }
        ViewSort::Title => tasks.sort_by(|a, b| {
            a.title
                .to_lowercase()
                .cmp(&b.title.to_lowercase())
                .then(a.id.cmp(&b.id))
        }),
        ViewSort::Priority => tasks.sort_by(|a, b| {
            let rank = |t: &Task| t.priority.unwrap_or(u8::MAX);
            rank(a).cmp(&rank(b)).then(a.id.cmp(&b.id))
        }),
        ViewSort::Due => tasks
            .sort_by(|a, b| by_time(a.due_at.as_ref(), b.due_at.as_ref()).then(a.id.cmp(&b.id))),
        ViewSort::Scheduled => tasks.sort_by(|a, b| {
            by_time(a.scheduled_at.as_ref(), b.scheduled_at.as_ref()).then(a.id.cmp(&b.id))
        }),
        ViewSort::Default | ViewSort::Unknown(_) => {}
    }
}

/// A saved view, as the vault holds it.
///
/// How each field merges (ADR-0044, declared in `sunrise_id::for_each_entity!`):
/// every one is its own register, written only by `Patch`. `name`, `view` and
/// `filter` are tracked, so concurrent edits of one of them are kept and
/// surfaced; `sort` and `sort_order` are quiet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedView {
    /// Unique id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: Timestamp,
    /// Last-update time (merge-derived).
    pub updated_at: Timestamp,
    /// The name it is recalled by. Not unique: two devices can each save one
    /// name while apart, and both views are kept.
    pub name: String,
    /// The primary view it opens.
    pub view: ViewKind,
    /// What it narrows the view's tasks to.
    #[serde(default)]
    pub filter: ViewFilter,
    /// How the tasks are ordered.
    #[serde(default)]
    pub sort: ViewSort,
    /// Its place in the saved-views list, a key of [`crate::sort_order`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_order: Option<String>,
    /// Deleted (tombstone).
    #[serde(default)]
    pub deleted: bool,
    /// Fields a newer build added, kept verbatim.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

/// A new saved view. The core mints the id and places it last in the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedViewDraft {
    /// The name.
    pub name: String,
    /// The view.
    pub view: ViewKind,
    /// The filter.
    #[serde(default)]
    pub filter: ViewFilter,
    /// The order.
    #[serde(default)]
    pub sort: ViewSort,
}

/// An edit of a saved view: each `Some` field is written, and only those, so
/// an edit of one field never overwrites a concurrent edit of another.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedViewPatch {
    /// A new name.
    pub name: Option<String>,
    /// A new view.
    pub view: Option<ViewKind>,
    /// A new filter, whole.
    pub filter: Option<ViewFilter>,
    /// A new order.
    pub sort: Option<ViewSort>,
    /// A new place in the list; `Some(None)` unplaces it.
    #[allow(clippy::option_option)]
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double")]
    pub sort_order: Option<Option<String>>,
}

/// `Option<Option<T>>` as absent (`None`), `null` (`Some(None)`) or a value,
/// so a patch that unplaces a view survives a round trip as one.
mod double {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[allow(clippy::option_option, clippy::ref_option)]
    pub(super) fn serialize<T: Serialize, S: Serializer>(
        v: &Option<Option<T>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match v {
            None | Some(None) => s.serialize_none(),
            Some(Some(v)) => v.serialize(s),
        }
    }

    #[allow(clippy::option_option)]
    pub(super) fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(d).map(Some)
    }
}

fn validate_filter(f: &ViewFilter) -> Result<(), ValidationError> {
    if let Some(p) = &f.priority {
        if !(1..=5).contains(&p.min) || !(1..=5).contains(&p.max) || p.min > p.max {
            return Err(ValidationError::Field {
                field: "saved_view.filter.priority",
                constraint: "range_1_5",
            });
        }
    }
    Ok(())
}

impl SavedViewDraft {
    /// Validate the draft.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let _ = validate_title(&self.name, "saved_view.name", MAX_VIEW_NAME_LEN)?;
        validate_filter(&self.filter)
    }
}

impl SavedViewPatch {
    /// Validate the fields the patch writes.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(n) = &self.name {
            let _ = validate_title(n, "saved_view.name", MAX_VIEW_NAME_LEN)?;
        }
        if let Some(f) = &self.filter {
            validate_filter(f)?;
        }
        if let Some(Some(k)) = &self.sort_order {
            crate::sort_order::validate_as(k, SORT_ORDER_FIELD)?;
        }
        Ok(())
    }

    /// Whether the patch writes nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.view.is_none()
            && self.filter.is_none()
            && self.sort.is_none()
            && self.sort_order.is_none()
    }
}

#[cfg(test)]
mod tests;

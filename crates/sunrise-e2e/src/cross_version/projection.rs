//! What the harness reads back out of a task: the fields it tracks, and which
//! of them a newer write sets.

use std::collections::BTreeMap;

use sunrise_domain::Task;

use super::future::{FutureWrite, FUTURE_FIELD};

/// A field the harness writes and reads back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Field {
    /// `title`.
    Title,
    /// `priority`.
    Priority,
    /// `deleted`.
    Deleted,
    /// `state`, as its raw spelling.
    State,
    /// The kind of `due_at`, when set.
    DueKind,
    /// The newer build's top-level field.
    Note,
    /// `stream_id`. Every task the scenario makes is created in the Inbox:
    /// by a baseline as the sentinel stream, by `HEAD` with no stream. Read
    /// by `HEAD` it is no stream either way (ADR-0046 §5).
    Stream,
}

impl Field {
    /// Every field, in projection order.
    pub const ALL: [Self; 7] = [
        Self::Title,
        Self::Priority,
        Self::Deleted,
        Self::State,
        Self::DueKind,
        Self::Note,
        Self::Stream,
    ];
}

impl FutureWrite {
    /// The field this newer write sets, if it sets one this build can read.
    #[must_use]
    pub const fn field(self) -> Option<Field> {
        match self {
            Self::Field => Some(Field::Note),
            Self::EnumValue => Some(Field::State),
            Self::NestedKind => Some(Field::DueKind),
            Self::OpKind => None,
        }
    }
}

/// One task as a replica holds it, field by field.
pub type Projection = BTreeMap<Field, Option<String>>;

/// Read the fields the harness tracks out of a `HEAD` `Task`.
#[must_use]
pub fn project(t: &Task) -> Projection {
    let mut p = Projection::new();
    p.insert(Field::Title, Some(t.title.clone()));
    p.insert(Field::Priority, t.priority.map(|n| n.to_string()));
    p.insert(Field::Deleted, Some(t.deleted.to_string()));
    p.insert(Field::State, Some(t.state.as_str().to_owned()));
    p.insert(
        Field::DueKind,
        t.hard_due_at.as_ref().map(|d| d.kind_str().to_owned()),
    );
    p.insert(
        Field::Note,
        t.unknown
            .get(FUTURE_FIELD)
            .and_then(|v| v.get().as_text().map(str::to_owned)),
    );
    p.insert(Field::Stream, t.stream_id.map(|s| s.to_str()));
    p
}

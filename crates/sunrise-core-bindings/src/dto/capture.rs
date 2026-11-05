//! Capture preview: what a quick-capture line parses into.

use sunrise_domain::capture::Unresolved;

use super::{Constraint, TaskDraftIn, TimeValue};

/// See [`sunrise_domain::capture::Unresolved`]: a token that looked like an
/// annotation and could not be applied.
///
/// Mirrored with named fields rather than declared remotely so the Swift cases
/// read as `.unknownStream(name:)` instead of the positional `v1` UniFFI gives
/// a tuple variant. Nothing is lost when one of these appears — the token's
/// text stays in the title — but the user is owed an explanation, and the
/// candidates on the ambiguous cases are what an inline picker offers.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum CaptureIssue {
    /// `#name` matched no live stream.
    UnknownStream {
        /// The typed name.
        name: String,
    },
    /// `#name` matched more than one live stream.
    AmbiguousStream {
        /// The typed name.
        typed: String,
        /// The names that matched.
        candidates: Vec<String>,
    },
    /// `@name` matched no live context.
    UnknownContext {
        /// The typed name.
        name: String,
    },
    /// `@name` matched more than one live context.
    AmbiguousContext {
        /// The typed name.
        typed: String,
        /// The names that matched.
        candidates: Vec<String>,
    },
    /// `^when` / `due:when` was not a date the parser understands.
    UnparseableDate {
        /// The typed text.
        text: String,
    },
    /// `!N` was outside 1..=5.
    PriorityOutOfRange {
        /// The typed text.
        text: String,
    },
    /// `~30m` / `~2h` was not a duration.
    UnparseableDuration {
        /// The typed text.
        text: String,
    },
}

impl From<&Unresolved> for CaptureIssue {
    fn from(u: &Unresolved) -> Self {
        match u {
            Unresolved::UnknownStream(name) => Self::UnknownStream { name: name.clone() },
            Unresolved::AmbiguousStream { typed, candidates } => Self::AmbiguousStream {
                typed: typed.clone(),
                candidates: candidates.clone(),
            },
            Unresolved::UnknownContext(name) => Self::UnknownContext { name: name.clone() },
            Unresolved::AmbiguousContext { typed, candidates } => Self::AmbiguousContext {
                typed: typed.clone(),
                candidates: candidates.clone(),
            },
            Unresolved::UnparseableDate(text) => Self::UnparseableDate { text: text.clone() },
            Unresolved::PriorityOutOfRange(text) => Self::PriorityOutOfRange { text: text.clone() },
            Unresolved::UnparseableDuration(text) => {
                Self::UnparseableDuration { text: text.clone() }
            }
        }
    }
}

/// What one capture line parsed to, without writing anything.
///
/// The draft is the *same value* [`crate::SunriseCore::capture`] would submit,
/// so a preview and the commit that follows it cannot disagree — the caller
/// hands this exact draft back as `CoreCommand::CreateTask`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CapturePreview {
    /// The task that would be created.
    pub draft: TaskDraftIn,
    /// Tokens that could not be applied. Their text is still in
    /// `draft.title`, so nothing typed is lost.
    pub issues: Vec<CaptureIssue>,
}

impl From<&sunrise_domain::capture::Capture> for CapturePreview {
    fn from(c: &sunrise_domain::capture::Capture) -> Self {
        let sunrise_domain::capture::Capture { draft, unresolved } = c;
        Self {
            draft: TaskDraftIn::from(draft),
            issues: unresolved.iter().map(CaptureIssue::from).collect(),
        }
    }
}

impl From<&sunrise_domain::TaskDraft> for TaskDraftIn {
    fn from(d: &sunrise_domain::TaskDraft) -> Self {
        let sunrise_domain::TaskDraft {
            title,
            body,
            stream_id,
            contexts,
            priority,
            energy,
            estimated_duration_s,
            planned_at,
            target_at,
            hard_due_at,
            scheduling_constraints,
            assignee,
            reminder_lead_s,
        } = d;
        Self {
            title: title.clone(),
            body: body.clone(),
            stream_id: *stream_id,
            contexts: contexts.clone(),
            priority: *priority,
            energy: energy.clone(),
            estimated_duration_s: *estimated_duration_s,
            scheduled_at: planned_at.as_ref().map(TimeValue::from),
            due_at: hard_due_at.as_ref().map(TimeValue::from),
            scheduling_constraints: scheduling_constraints
                .iter()
                .map(Constraint::from)
                .collect(),
            assignee: *assignee,
            reminder_lead_s: *reminder_lead_s,
            target_at: target_at.as_ref().map(TimeValue::from),
        }
    }
}

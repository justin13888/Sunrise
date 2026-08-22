//! Sunrise domain entities and validation.
//!
//! Implements `docs/02-domain/`. Each entity lives in its own module; the
//! module-level docs cite the relevant spec file. Validation rules return
//! [`sunrise_error::ErrorCode`] values so the core can surface them to the
//! UI without translation.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// Domain entities use a lot of CBOR-mapped optional fields; relax the
// stylistic lints while the layer stabilizes (revisit Phase 17).
#![allow(
    clippy::doc_markdown,
    clippy::manual_let_else,
    clippy::too_many_lines,
    clippy::missing_errors_doc,
    clippy::missing_const_for_fn,
    clippy::struct_excessive_bools
)]

pub mod attachment;
pub mod block;
pub mod capture;
pub mod common;
pub mod constraint;
pub mod context;
pub mod deps;
pub mod inbox;
pub mod note;
pub mod person;
pub mod routine;
pub mod routine_gen;
pub mod rrule;
pub mod schema;
pub mod streak;
pub mod stream;
pub mod task;
pub mod validation;

pub use attachment::Attachment;
pub use block::{Block, BlockDraft};
pub use common::{Energy, NoteBody};
pub use constraint::{
    validate_list as validate_constraint_list, violations_by_severity, ConstraintError,
    ConstraintSeverity, DateRange, ScheduleConstraint, TimeOfDayRange, WeekdaySet, MAX_CONSTRAINTS,
};
pub use context::{
    Context, ContextDraft, ContextFacet, ContextPatch, ENERGY_PREFIX, MAX_CONTEXT_DESCRIPTION_LEN,
    WAITING_ON_PREFIX,
};
pub use deps::{
    blocker_is_open, effective_state, is_actionable, DependencyGraph, EffectiveTaskState,
};
pub use inbox::{inbox_stream_ref, INBOX_STREAM_BYTES, INBOX_STREAM_ID};
pub use note::Note;
pub use person::Person;
pub use routine::{
    materialization_horizon_days, Routine, RoutineCatchupPolicy, RoutineDraft, RoutinePatch,
    RoutineReviewCadence, TaskTemplate,
};
pub use routine_gen::{expand, occurrence_key_at, occurrence_task_id, ExpandError, Occurrence};
pub use rrule::{Frequency, RRule, RRuleParseError, Weekday};
pub use schema::DOC_SCHEMA_V;
pub use streak::{
    StreakOutcome, DEFAULT_GRACE_WINDOW_S, FORGIVENESS_ALLOWANCE, FORGIVENESS_WINDOW_S,
    MAX_GRACE_WINDOW_S,
};
pub use stream::{Stream, StreamColor, StreamDraft, StreamPatch, StreamReviewCadence};
pub use task::{Task, TaskDraft, TaskPatch, TaskState};
pub use validation::{
    ValidationError, MAX_CONTEXT_NAME_LEN, MAX_TASK_ENVELOPE_BYTES, MAX_TASK_TITLE_LEN,
};

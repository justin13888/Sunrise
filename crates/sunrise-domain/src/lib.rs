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
pub mod common;
pub mod constraint;
pub mod context;
pub mod inbox;
pub mod note;
pub mod person;
pub mod routine;
pub mod rrule;
pub mod schema;
pub mod stream;
pub mod task;
pub mod validation;

pub use attachment::Attachment;
pub use block::{Block, BlockDraft};
pub use common::{Energy, NoteBody};
pub use constraint::{
    validate_list as validate_constraint_list, ConstraintError, ConstraintSeverity, DateRange,
    ScheduleConstraint, TimeOfDayRange, WeekdaySet, MAX_CONSTRAINTS,
};
pub use context::Context;
pub use inbox::{inbox_stream_ref, INBOX_STREAM_BYTES, INBOX_STREAM_ID};
pub use note::Note;
pub use person::Person;
pub use routine::{
    Routine, RoutineCatchupPolicy, RoutineDraft, RoutineReviewCadence, TaskTemplate,
};
pub use rrule::{Frequency, RRule, RRuleParseError, Weekday};
pub use schema::DOC_SCHEMA_V;
pub use stream::{Stream, StreamColor, StreamDraft, StreamPatch, StreamReviewCadence};
pub use task::{Task, TaskDraft, TaskPatch, TaskState};
pub use validation::{ValidationError, MAX_TASK_ENVELOPE_BYTES, MAX_TASK_TITLE_LEN};

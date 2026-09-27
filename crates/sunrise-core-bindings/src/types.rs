//! Scalars and unit enums, taught to cross the FFI boundary.
//!
//! Two mechanisms, chosen per type:
//!
//! * [`uniffi::custom_type!`] for the handful of scalars that already have a
//!   lossless string or integer form — ids, instants, civil dates. The foreign
//!   side sees the primitive; Rust keeps the type.
//! * `#[uniffi::remote(Enum)]` for the unit enums. It works across crate
//!   boundaries, which is the whole reason **`sunrise-domain` gains no uniffi
//!   dependency**, and it is *drift-proof*: the declaration below has to match
//!   the real enum variant for variant, so adding a variant upstream fails
//!   this crate's build rather than silently producing an unrepresentable
//!   value at runtime.
//!
//! Structured types are mirrored by hand in [`crate::dto`] instead — see the
//! rationale there.

use sunrise_domain::{
    ConstraintSeverity, EffectiveTaskState, Energy, EnergyFit, ExportDataset, ExportFormat,
    FocusKind, Frequency, InterruptionReason, NoteBody, QuietHoursPolicy, ReminderKind,
    RoutineCatchupPolicy, SessionLength, SnoozeSpan, StreamColor, StreamReviewCadence, TaskState,
    UnknownVariant, Weekday,
};
use sunrise_id::EntityRef;
use sunrise_sync::SyncState;

use crate::BindingError;

// `custom_type!` takes a single-component path, so the jiff types are imported
// under local names rather than spelled out.
use jiff::civil::{Date as CivilDate, DateTime as CivilDateTime, Time as CivilTime};
use jiff::Timestamp;

// ---------------------------------------------------------------------------
// Scalars
// ---------------------------------------------------------------------------

// An `EntityRef` is a kind tag plus 16 bytes, and UniFFI has no fixed-size
// array type at all. It already round-trips through `Display`/`FromStr` as
// `tsk_01ARZ3…`, which is what the user sees and what the CLI accepts, so the
// foreign side gets that string and nothing is invented.
uniffi::custom_type!(EntityRef, String, {
    remote,
    lower: |r| r.to_str(),
    try_lift: |s: String| {
        EntityRef::parse_any(&s)
            .map_err(|e| BindingError::BadId { id: s.clone(), cause: e.to_string() }.into())
    },
});

// Epoch milliseconds, the same unit `Core::now_ms` speaks.
uniffi::custom_type!(Timestamp, i64, {
    remote,
    lower: |t| t.as_millisecond(),
    try_lift: |ms: i64| {
        Timestamp::from_millisecond(ms)
            .map_err(|e| BindingError::BadTime { value: ms.to_string(), cause: e.to_string() }.into())
    },
});

// Civil (zone-less) values keep their ISO-8601 text. `jiff`'s `Display` and
// `FromStr` are exact inverses for all three, so this is lossless — and the
// text is what a `SunriseTime::Zoned` means to a human anyway.
uniffi::custom_type!(CivilDateTime, String, {
    remote,
    lower: |d| d.to_string(),
    try_lift: |s: String| {
        s.parse::<CivilDateTime>()
            .map_err(|e| BindingError::BadTime { value: s.clone(), cause: e.to_string() }.into())
    },
});
uniffi::custom_type!(CivilDate, String, {
    remote,
    lower: |d| d.to_string(),
    try_lift: |s: String| {
        s.parse::<CivilDate>()
            .map_err(|e| BindingError::BadTime { value: s.clone(), cause: e.to_string() }.into())
    },
});
uniffi::custom_type!(CivilTime, String, {
    remote,
    lower: |t| t.to_string(),
    try_lift: |s: String| {
        s.parse::<CivilTime>()
            .map_err(|e| BindingError::BadTime { value: s.clone(), cause: e.to_string() }.into())
    },
});

// A note body is opaque bytes to everything but the editor that wrote it.
uniffi::custom_type!(NoteBody, Vec<u8>, {
    remote,
    lower: |b| b.0,
    try_lift: |v: Vec<u8>| Ok(NoteBody(v)),
});

// The raw spelling of an enum value this build does not recognise
// (ADR-0045 §6). The foreign side sees the string, renders the value as
// "unknown", and hands it back unchanged so a write carries it through.
uniffi::custom_type!(UnknownVariant, String, {
    remote,
    lower: |u| u.as_str().to_owned(),
    try_lift: |s: String| Ok(UnknownVariant::new(&s)),
});

// ---------------------------------------------------------------------------
// Enums, declared remotely
// ---------------------------------------------------------------------------
//
// Every enum that crosses the wire or storage carries an `Unknown` case
// holding the raw value a newer client wrote (ADR-0045 §6). A client renders
// it as "unknown", never as the fallback the core's logic reads it as, and
// passes it back untouched.

/// See [`sunrise_domain::TaskState`].
#[uniffi::remote(Enum)]
pub enum TaskState {
    Todo,
    InProgress,
    Done,
    Cancelled,
    Unknown(UnknownVariant),
}

/// See [`sunrise_domain::EffectiveTaskState`] — the read-time widening of
/// [`TaskState`] with the derived `Blocked` case.
#[uniffi::remote(Enum)]
pub enum EffectiveTaskState {
    Todo,
    InProgress,
    Blocked,
    Done,
    Cancelled,
}

/// See [`sunrise_domain::Energy`].
#[uniffi::remote(Enum)]
pub enum Energy {
    Low,
    Med,
    High,
    Unknown(UnknownVariant),
}

/// See [`sunrise_domain::StreamColor`].
#[uniffi::remote(Enum)]
pub enum StreamColor {
    Slate,
    Rose,
    Amber,
    Emerald,
    Sky,
    Indigo,
    Violet,
    Pink,
    Unknown(UnknownVariant),
}

/// See [`sunrise_domain::StreamReviewCadence`].
#[uniffi::remote(Enum)]
pub enum StreamReviewCadence {
    Weekly,
    Biweekly,
    Monthly,
    None,
    Unknown(UnknownVariant),
}

/// See [`sunrise_domain::RoutineCatchupPolicy`].
#[uniffi::remote(Enum)]
pub enum RoutineCatchupPolicy {
    Skip,
    Merge,
    Queue,
    Unknown(UnknownVariant),
}

/// See [`sunrise_domain::FocusKind`].
#[uniffi::remote(Enum)]
pub enum FocusKind {
    Work,
    Break,
    Unknown(UnknownVariant),
}

/// See [`sunrise_domain::SessionLength`].
#[uniffi::remote(Enum)]
pub enum SessionLength {
    OnePomodoro,
    SizedToEstimate,
    UntilDone,
}

/// See [`sunrise_domain::InterruptionReason`].
#[uniffi::remote(Enum)]
pub enum InterruptionReason {
    SelfInterrupt,
    Meeting,
    Blocked,
    Other,
    Unknown(UnknownVariant),
}

/// See [`sunrise_domain::EnergyFit`].
#[uniffi::remote(Enum)]
pub enum EnergyFit {
    Exact,
    Unknown,
    Under,
    Over,
}

/// See [`sunrise_domain::ConstraintSeverity`].
#[uniffi::remote(Enum)]
pub enum ConstraintSeverity {
    Hard,
    Soft,
    Unknown(UnknownVariant),
}

/// See [`sunrise_domain::ExportFormat`].
#[uniffi::remote(Enum)]
pub enum ExportFormat {
    Csv,
    Json,
}

/// See [`sunrise_domain::ExportDataset`].
#[uniffi::remote(Enum)]
pub enum ExportDataset {
    Trends,
    Activity,
    Focus,
    Streaks,
}

/// See [`sunrise_domain::Frequency`].
#[uniffi::remote(Enum)]
pub enum Frequency {
    Daily,
    Weekly,
    Monthly,
    Yearly,
    Unknown(UnknownVariant),
}

/// See [`sunrise_domain::Weekday`].
#[uniffi::remote(Enum)]
pub enum Weekday {
    Mo,
    Tu,
    We,
    Th,
    Fr,
    Sa,
    Su,
    Unknown(UnknownVariant),
}

/// See [`sunrise_sync::SyncState`].
#[uniffi::remote(Enum)]
pub enum SyncState {
    Disconnected,
    CatchingUp,
    Live,
    Degraded,
    Stopped,
}

/// See [`sunrise_domain::ReminderKind`] — which of the spec's notification
/// sources an intent came from.
#[uniffi::remote(Enum)]
pub enum ReminderKind {
    Task,
    Block,
    Routine,
}

/// See [`sunrise_domain::QuietHoursPolicy`].
#[uniffi::remote(Enum)]
pub enum QuietHoursPolicy {
    Queue,
    Drop,
}

/// See [`sunrise_domain::SnoozeSpan`] — how far a "not now" pushes something
/// out. Two of the three are civil, not durations.
#[uniffi::remote(Enum)]
pub enum SnoozeSpan {
    OneHour,
    Tomorrow,
    NextWeek,
}

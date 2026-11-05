//! The ways a run can break the invariant, where each was seen, and the known
//! gap that explains it.

use sunrise_id::EntityRef;

use super::future::FutureWrite;
use super::projection::Field;

impl FutureWrite {
    /// The issue that tracks a baseline dropping this write.
    #[must_use]
    pub const fn baseline_issue(self) -> Option<u32> {
        match self {
            Self::Field => None,
            Self::EnumValue => Some(321),
            Self::NestedKind => Some(322),
            Self::OpKind => Some(320),
        }
    }
}

/// Which replica a violation was seen on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Who {
    /// A before its upgrade.
    A,
    /// A after it was reopened by `HEAD`.
    AUpgraded,
    /// B. For a logged corruption, any `HEAD` core: they share one process
    /// and one log.
    B,
}

/// One way a run broke the invariant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// `HEAD` would not open a vault the baseline wrote.
    UpgradeRefused {
        /// Which vault: B at setup, or A at the end.
        who: Who,
        /// The open's refusal.
        error: String,
    },
    /// A replica refused a newer op handed to it directly.
    Refused {
        /// Where.
        who: Who,
        /// The task the op was about.
        task: EntityRef,
        /// What it set.
        write: FutureWrite,
        /// Newer values it carried from earlier newer writes to the task.
        /// Each op is a whole task as `HEAD` held it, so it carries every
        /// unknown value `HEAD` kept, and any of them can be what the replica
        /// could not read.
        carried: Vec<FutureWrite>,
        /// Whether the refusal is the one the sync driver counts as corruption.
        corruption: bool,
        /// The refusal.
        error: String,
    },
    /// A replica's sync driver logged an inbound op as corruption.
    LoggedCorruption {
        /// Where.
        who: Who,
        /// The message.
        message: String,
    },
    /// A replica logged an error or a warning.
    LoggedError {
        /// Where.
        who: Who,
        /// `ev` and message.
        event: String,
    },
    /// A command the scenario issued failed.
    CommandFailed {
        /// Where.
        who: Who,
        /// What was asked.
        command: String,
        /// The failure.
        error: String,
    },
    /// A replica stopped syncing.
    StoppedSyncing {
        /// Where.
        who: Who,
        /// What was seen.
        detail: String,
    },
    /// A replica never materialized a task.
    Missing {
        /// Where.
        who: Who,
        /// The task.
        task: EntityRef,
    },
    /// A replica holds a value for a field that no last write gave it.
    Lost {
        /// Where.
        who: Who,
        /// The task.
        task: EntityRef,
        /// The field.
        field: Field,
        /// Every acceptable value.
        expected: Vec<Option<String>>,
        /// What the replica holds.
        got: Option<String>,
    },
}

/// A violation and the known gap that explains it, if one does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    /// The violation.
    pub violation: Violation,
    /// The issue of the known gap that accounts for it, or `None`.
    pub issue: Option<u32>,
}

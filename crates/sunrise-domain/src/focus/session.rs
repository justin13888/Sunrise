//! The ADR-0013 entity layer: the two append-only ops, the interruptions
//! logged against them, and the read-side view assembled from both.
//!
//! These are the only serde-mapped wire records in [`crate::focus`], and the
//! only readers of [`crate::epoch_ms`] and [`crate::unknown::Unknowns`] —
//! every instant below is a `Timestamp` in memory and an integer count of
//! milliseconds under its original `*_ms` key on the wire, and every entity
//! but [`Interruption`] carries the unknown-field map forward compatibility
//! needs. They belong together because they are what is *written*: nothing
//! here decides a policy, and the timer, the planner and the calibration fold
//! all take these values as their input.

use crate::common::Energy;
use crate::epoch_ms;
use crate::unknown::Unknowns;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sunrise_id::EntityRef;

// ---------------------------------------------------------------------------
// Scalars
// ---------------------------------------------------------------------------

/// What a session *is*: focused work, or the break that follows it. Breaks are
/// recorded so the timeline reconstructs, but they never count as focused time
/// and never feed estimate calibration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusKind {
    /// A work segment.
    Work,
    /// A break segment.
    Break,
}

impl FocusKind {
    /// Parse from the wire/storage string. An unrecognised value degrades to
    /// [`FocusKind::Work`] rather than failing.
    ///
    /// Focus sessions default to work; counting an unknown kind as a break would
    /// under-report time actually spent.
    #[must_use]
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "break" => Self::Break,
            // "work" and anything this build has never heard of.
            _ => Self::Work,
        }
    }
}

crate::unknown::lossy_enum!(FocusKind);

impl FocusKind {
    /// Wire/storage tag.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Break => "break",
        }
    }

    /// Parse a storage tag. `None` for anything else.
    #[must_use]
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "work" => Some(Self::Work),
            "break" => Some(Self::Break),
            _ => None,
        }
    }
}

/// A one-tap reason for bailing out early or switching task
/// (`docs/08-features/focus-mode.md` §Interruption capture). Data the user
/// opted into; there is no shame UI and no score attached to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptionReason {
    /// The user interrupted themselves.
    SelfInterrupt,
    /// A meeting or another person.
    Meeting,
    /// The work turned out to be blocked.
    Blocked,
    /// Anything else.
    Other,
}

impl Interruption {
    /// Interruption instant as epoch milliseconds.
    #[must_use]
    pub fn at_ms(&self) -> u64 {
        epoch_ms::to_u64(self.at)
    }
}

impl InterruptionReason {
    /// Wire/storage tag.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SelfInterrupt => "self",
            Self::Meeting => "meeting",
            Self::Blocked => "blocked",
            Self::Other => "other",
        }
    }

    /// Parse a storage tag; unknown values degrade to [`Self::Other`] rather
    /// than failing a read — an interruption reason is never load-bearing.
    #[must_use]
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "self" => Self::SelfInterrupt,
            "meeting" => Self::Meeting,
            "blocked" => Self::Blocked,
            _ => Self::Other,
        }
    }
}

crate::unknown::lossy_enum!(InterruptionReason);

/// "Chunk N of M" — the checkpoint marker shown when a task's estimate exceeds
/// one session, so a long task shows visible progress *within* a sitting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    /// 1-based index of this sitting.
    pub index: u32,
    /// Total sittings the estimate implies.
    pub total: u32,
}

// ---------------------------------------------------------------------------
// The two ops
// ---------------------------------------------------------------------------

/// The `start` op: everything known when a session begins. Immutable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusStart {
    /// This session's own id (`fcs_`), minted at start.
    pub id: EntityRef,
    /// Task being focused on.
    pub task_id: EntityRef,
    /// Owning Stream (the op's routing stream, and the stats bucket).
    pub stream_id: EntityRef,
    /// Wall clock at start, from the injected clock.
    ///
    /// A `Timestamp` rather than a bare `u64`, so it cannot be confused with
    /// the DURATIONS beside it. The wire form is unchanged — still an integer
    /// count of milliseconds under the same `started_at_ms` key.
    #[serde(rename = "started_at_ms", with = "crate::epoch_ms")]
    pub started_at: Timestamp,
    /// Planned length, `None` for an open-ended `until done` session.
    #[serde(default)]
    pub planned_ms: Option<u64>,
    /// The session's declared **energy budget** — what the user has in the
    /// tank right now, which the planner matches work against.
    #[serde(default)]
    pub energy: Option<Energy>,
    /// Work or break.
    pub kind: FocusKind,
    /// "Chunk N of M", when the task's estimate exceeds one session.
    #[serde(default)]
    pub chunk: Option<Chunk>,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted. See [`crate::unknown`] and
    /// `docs/10-cross-cutting/protocol-versioning.md` §7.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

impl FocusStart {
    /// Start instant as epoch milliseconds — the form storage columns and the
    /// stats grid still speak.
    #[must_use]
    pub fn started_at_ms(&self) -> u64 {
        epoch_ms::to_u64(self.started_at)
    }
}

/// The `end` op: the frozen tail of a session. Immutable, and addressed to the
/// same session id as its [`FocusStart`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusEnd {
    /// The session being closed.
    pub session_id: EntityRef,
    /// Wall clock at end. Wire form unchanged (integer ms, `ended_at_ms`).
    #[serde(rename = "ended_at_ms", with = "crate::epoch_ms")]
    pub ended_at: Timestamp,
    /// Focused time this session actually contained — **computed and frozen
    /// here**, never stored while the session runs.
    ///
    /// Stays `u64` milliseconds: it is a DURATION, not an instant, and the
    /// distinction is exactly what having one type for both used to hide.
    pub actual_focused_ms: u64,
    /// Interruptions the ending device knows about. Set-union'd with any
    /// [`Interruption`] ops on merge; never a replacement.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interruptions: Vec<Interruption>,
    /// Whether the task was completed in this session.
    pub completed_task: bool,
    /// Fields written by a newer `DOC_SCHEMA_V` that this build does not
    /// model, preserved verbatim and re-emitted. See [`crate::unknown`] and
    /// `docs/10-cross-cutting/protocol-versioning.md` §7.
    #[serde(flatten)]
    pub unknown: Unknowns,
}

impl FocusEnd {
    /// End instant as epoch milliseconds.
    #[must_use]
    pub fn ended_at_ms(&self) -> u64 {
        epoch_ms::to_u64(self.ended_at)
    }
}

/// One logged interruption. Its own append-only record: the `(session, at,
/// reason)` triple is the primary key, so re-delivery is idempotent and two
/// devices logging different interruptions on one session both survive.
///
/// The ONE entity with no `unknown` map. It has no identity apart from its
/// three fields — the whole triple IS the key — so a preserved unknown field
/// would change what the record *is* rather than what it says, and would give
/// two records that a peer considers the same one different identities here.
/// It is `Copy` and totally ordered for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Interruption {
    /// Session interrupted.
    pub session_id: EntityRef,
    /// When it happened. Wire form unchanged (integer ms, `at_ms`).
    #[serde(rename = "at_ms", with = "crate::epoch_ms")]
    pub at: Timestamp,
    /// One-tap reason.
    pub reason: InterruptionReason,
}

// ---------------------------------------------------------------------------
// The read-side view
// ---------------------------------------------------------------------------

/// A session as a reader sees it: its immutable start, its `end` if one has
/// arrived, and the union of its interruptions.
///
/// This is a *view*, assembled from the two append-only records — it is never
/// itself written anywhere, which is what keeps a running timer out of storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusSession {
    /// The `start` op.
    pub start: FocusStart,
    /// The `end` op, if it has arrived. `None` means **still running** — a
    /// valid state, not a broken one.
    #[serde(default)]
    pub end: Option<FocusEnd>,
    /// Every interruption logged against this session, deduped and ordered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interruptions: Vec<Interruption>,
}

impl FocusSession {
    /// A session with no `end` op is still running. The app dying mid-session
    /// leaves exactly this state and it needs no repair: an `end` op may arrive
    /// later from any device.
    #[must_use]
    pub const fn is_running(&self) -> bool {
        self.end.is_none()
    }

    /// Wall-clock span of the session, **derived, never stored**.
    ///
    /// Running: `now_ms - started_at`. Ended: `ended_at - started_at`. A clock
    /// that reads behind the start (skew, or a stale `now_ms`) saturates to
    /// zero rather than wrapping.
    #[must_use]
    pub fn elapsed_ms(&self, now_ms: u64) -> u64 {
        match &self.end {
            Some(e) => e.ended_at_ms().saturating_sub(self.start.started_at_ms()),
            None => now_ms.saturating_sub(self.start.started_at_ms()),
        }
    }

    /// Focused time: the frozen `actual_focused_ms` once the session has ended,
    /// and the derived elapsed while it runs.
    #[must_use]
    pub fn focused_ms(&self, now_ms: u64) -> u64 {
        match &self.end {
            Some(e) => e.actual_focused_ms,
            None => self.elapsed_ms(now_ms),
        }
    }

    /// Remaining time against the planned length; `None` for an open-ended
    /// (`until done`) session, `Some(0)` once the plan is spent.
    #[must_use]
    pub fn remaining_ms(&self, now_ms: u64) -> Option<u64> {
        self.start
            .planned_ms
            .map(|p| p.saturating_sub(self.elapsed_ms(now_ms)))
    }

    /// Whether a planned session has run past its plan.
    #[must_use]
    pub fn overran(&self, now_ms: u64) -> bool {
        self.remaining_ms(now_ms) == Some(0) && self.start.planned_ms.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::timer::POMODORO_MS;
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

    fn start(id: u8, task: u8, at: u64) -> FocusStart {
        FocusStart {
            id: fcs(id),
            task_id: tsk(task),
            stream_id: strm(1),
            started_at: epoch_ms::from_u64(at),
            planned_ms: Some(POMODORO_MS),
            energy: Some(Energy::High),
            kind: FocusKind::Work,
            chunk: None,
            unknown: Unknowns::new(),
        }
    }

    // ---- the ADR's two load-bearing properties ----

    #[test]
    fn a_dangling_start_reads_as_running() {
        // The app died mid-session: a `start` op with no `end`. That is a
        // valid state, not a tombstone case.
        let s = FocusSession {
            start: start(1, 1, 10_000),
            end: None,
            interruptions: Vec::new(),
        };
        assert!(s.is_running());
        assert_eq!(s.focused_ms(10_000 + 90_000), 90_000);
        // An `end` arriving later — from any device — closes it, and nothing
        // had to be repaired in between.
        let closed = FocusSession {
            end: Some(FocusEnd {
                session_id: fcs(1),
                ended_at: epoch_ms::from_u64(10_000 + 120_000),
                actual_focused_ms: 100_000,
                interruptions: Vec::new(),
                completed_task: true,
                unknown: Unknowns::new(),
            }),
            ..s
        };
        assert!(!closed.is_running());
    }

    #[test]
    fn elapsed_derives_from_the_clock_and_is_never_stored() {
        let s = FocusSession {
            start: start(1, 1, 1_000),
            end: None,
            interruptions: Vec::new(),
        };
        // Same session value, three different clocks, three different answers.
        assert_eq!(s.elapsed_ms(1_000), 0);
        assert_eq!(s.elapsed_ms(61_000), 60_000);
        assert_eq!(s.elapsed_ms(1_000 + POMODORO_MS), POMODORO_MS);
        // A clock reading behind the start saturates instead of wrapping.
        assert_eq!(s.elapsed_ms(0), 0);
        // Remaining is likewise derived, and bottoms out at zero.
        assert_eq!(s.remaining_ms(1_000), Some(POMODORO_MS));
        assert_eq!(s.remaining_ms(1_000 + 60_000), Some(POMODORO_MS - 60_000));
        assert_eq!(s.remaining_ms(u64::MAX), Some(0));
        assert!(s.overran(u64::MAX));
        assert!(!s.overran(1_000));

        // Once ended, the frozen value wins and the clock stops mattering.
        let ended = FocusSession {
            start: start(1, 1, 1_000),
            end: Some(FocusEnd {
                session_id: fcs(1),
                ended_at: epoch_ms::from_u64(1_000 + 300_000),
                actual_focused_ms: 240_000,
                interruptions: Vec::new(),
                completed_task: false,
                unknown: Unknowns::new(),
            }),
            interruptions: Vec::new(),
        };
        assert_eq!(ended.elapsed_ms(u64::MAX), 300_000);
        assert_eq!(ended.focused_ms(u64::MAX), 240_000);
    }

    #[test]
    fn open_ended_sessions_have_no_remaining() {
        let s = FocusSession {
            start: FocusStart {
                planned_ms: None,
                ..start(1, 1, 0)
            },
            end: None,
            interruptions: Vec::new(),
        };
        assert_eq!(s.remaining_ms(999_999), None);
        assert!(!s.overran(999_999));
    }

    #[test]
    fn scalar_tags_round_trip() {
        for k in [FocusKind::Work, FocusKind::Break] {
            assert_eq!(FocusKind::from_str_opt(k.as_str()), Some(k));
        }
        assert_eq!(FocusKind::from_str_opt("nope"), None);
        for r in [
            InterruptionReason::SelfInterrupt,
            InterruptionReason::Meeting,
            InterruptionReason::Blocked,
            InterruptionReason::Other,
        ] {
            assert_eq!(InterruptionReason::from_str_lossy(r.as_str()), r);
        }
        // Unknown reasons degrade rather than fail a read.
        assert_eq!(
            InterruptionReason::from_str_lossy("garbage"),
            InterruptionReason::Other
        );
    }
}

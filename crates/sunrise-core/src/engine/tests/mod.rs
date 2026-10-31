//! The engine's unit tests.
//!
//! Each theme is a module of its own, declared below. This file holds the
//! imports they share through `use super::*`, and every test draws on
//! [`support`].

use super::*;
use crate::config::SystemRng;
use crate::events::DomainEvent;
use parking_lot::Mutex as PLMutex;
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::ActivityKind;
use sunrise_domain::EffectiveTaskState;
use sunrise_domain::{EndOfDayPlan, MorningSummary};
use sunrise_domain::{RRule, Routine, RoutineCatchupPolicy, RoutineDraft, TaskTemplate};
use sunrise_storage::Db;
use support::*;
// The engine's own imports, which `use super::*` used to supply from the
// single-file `engine.rs`, plus one glob per sibling module of the split.
use super::attachment::*;
use super::block::*;
use super::context::*;
use super::focus::*;
use super::identity::{
    SiblingRank, MAX_ROSTER_ENTRIES, MAX_SIBLINGS_PER_PREDECESSOR, MAX_SIBLING_CANDIDATES,
    SIBLING_ORDER_DESC,
};
use super::ids::*;
use super::lww::*;
use super::oplog::*;
use super::routine::*;
use super::stream::*;
use super::task::*;
use crate::commands::{Command, CommandResult, FocusStartDraft};
use crate::config::Clock;
use crate::control_op::{
    DeviceRevokePayload, IdentityTransitionPayload, KeyEnvelopePayload, KeyShare, Recipient,
    RevokeReason, RosterEntry,
};
use crate::inner_op::{decode_inner_op, encode_inner_op, InnerOp};
use crate::keychain::{to16, KeySource, Keychain};
use crate::queries::{ActionableTask, BlockRow, ContextRow, FocusSessionRow, Query, QueryResult};
use rusqlite::{params, OptionalExtension};
use std::collections::BTreeSet;
use std::sync::Arc;
use sunrise_cbor::hlc::Hlc;
use sunrise_crypto::DeviceCert;
use sunrise_crypto::{stream_key_id, StreamKey};
use sunrise_domain::sort_order;
use sunrise_domain::time::SunriseTime;
use sunrise_domain::Unknowns;
use sunrise_domain::{
    imported_block_id, inbox_stream_ref, occurrence_key_at, occurrence_task_id, ActivityEvent,
    Attachment, AttachmentDraft, BlockDraft, BlockPatch, Chunk, Context, ContextDraft,
    ContextPatch, Energy, ExportDataset, ExportFormat, FocusEnd, FocusKind, FocusStart,
    InterruptionReason, NoteBody, ReminderSettings, ReviewSnapshotDraft, ReviewTotals,
    RoutinePatch, ScheduleConstraint, SessionLength, Stream, StreamColor, StreamDraft, StreamPatch,
    StreamReviewCadence, Task, TaskDraft, TaskPatch, TaskState, Trends, ValidationError,
    WeeklyReview, INBOX_STREAM_BYTES, POMODORO_MS,
};
use sunrise_id::{EntityKind, EntityRef};
use sunrise_storage::{OpLog, Outbox};

mod support;

mod feature_gate;

/// Per-field merge (ADR-0044): `Patch` ops and their convergence.
mod field_merge;

/// The Preferences entity and the device overlay (ADR-0050).
mod preferences;

/// Per-device op chains, fork evidence and the stream digest (ADR-0043).
mod op_chain;

/// Op-log compaction below an acknowledged floor, and stream snapshots
/// (ADR-0059).
mod compaction;

/// An attachment's thumbnail and dimensions (ADR-0053, issue #346).
mod attachment_thumbnail;

/// Today, the calendar windows and validation in the reader's zone
/// (`docs/10-cross-cutting/time.md`, issue #336).
mod time_zones;

/// The traveller harness: routines, reminders and constraints across a
/// scripted itinerary and a peer at home (ADR-0066).
mod traveller;

/// An inner op nested past `sunrise_cbor::MAX_DOC_DEPTH` parks; one at it
/// applies (ADR-0045 §4).
mod nesting_bound;

/// Rebuilding the projection from the op log (issue #441, ADR-0065 PR 2).
mod rebuild;

/// One merge path for every local write (ADR-0065 PR 3).
mod local_write;

/// The Upcoming view's civil days, span and no-date lane (#351).
mod upcoming;

/// The planner day: Today, Upcoming and the briefs on the day schedule's
/// bounds (#338).
mod day_schedule;

/// The version DAG: parents, frontiers and heads (ADR-0065 PR 4).
mod versions;

/// Pruning the version DAG below what every device has passed (ADR-0065
/// D25, #581).
mod prune;

/// Search over every indexed kind, and the indexes' upkeep (issue #345,
/// ADR-0052).
mod search;

/// The register fold by conflict class, the conflicts query, and resolve and
/// restore (ADR-0065 PR 5).
mod conflicts;

/// Groups: a commit folded whole or not at all (ADR-0065 §6, PR 6).
mod groups;

/// Manual task order: `MoveTasks` and the listings that honour it (#340).
mod task_order;

/// Saved views, a synced entity written only by `Patch` (#341).
mod saved_views;

/// The account's key bundle: what a replica publishes and absorbing one
/// (ADR-0069, #580).
mod key_bundle;

/// A recovery code across an identity rotation (ADR-0070, #583).
mod recovery_rotation;

/// Values `project` derives, the read-time cycle rules, and the history
/// readers that understand `Patch` (ADR-0065 PR 7).
mod derived;

/// `core.field_ops` and the write mode: every command writes the `Patch` of
/// the fields it changes once the vault requires it (ADR-0065 PR 8).
mod field_ops;

/// Optional streams, the private key domain, the Inbox view and stream
/// deletion (ADR-0046, #332).
mod optional_stream;

/// A task's three times and its lateness (ADR-0047, #334).
mod lateness;

/// The triage queue and its four actions (ADR-0047, #335).
mod triage;

/// A routine's series: edit scopes and the split, template propagation,
/// catch-up over occurrences generated earlier, and the hard-constraint flag
/// (#331).
mod series;

/// The notification catalog's reads: security events, deadlines, the
/// outbox's age, routine reminders (#347).
mod security_events;

/// Tasks: the command surface, the rows it writes and the Today query.
mod tasks;

/// Streams: their commands, order, counts and the fields they carry.
mod streams;

/// The vault-meta stream: its keys, and what no command may aim at it.
mod vault_meta;

/// Contexts: their commands, their tasks and their convergence.
mod contexts;

/// The activity feed and the timelines read back from the op log.
mod activity;

/// A task's scheduling constraints and what they gate.
mod task_constraints;

/// Task dependencies: derived `blocked`, the reverse index and the cascade.
mod task_dependencies;

/// Routines: materialization, its determinism and the streak.
mod routines;

/// Time blocks and the blocks an import writes.
mod blocks;

/// Attachments: their metadata, routing and tombstones.
mod attachments;

/// Focus sessions, their measurements and the completion they can carry.
mod focus;

/// The planner's ranking and the session each row would open.
mod planner;

/// Parked ops: what parks, what releases it and the schema fingerprint.
mod parked_ops;

/// The hybrid clock: restored at open, its drift window and the stamp observed.
mod clock;

/// The receive half of sync: envelope refusals and convergence.
mod apply_remote;

/// Device certificates: who may publish one, backfill and the readmission mark.
mod device_admission;

/// What a revocation does to stream keys and envelopes.
mod revocation_keys;

/// The revocation register: the fold, its gate and the discount.
mod revocation_fold;

/// Revocation against the apply cursor: what advances it and what does not.
mod revocation_cursor;

/// The read bound beside the register, and a gated revocation doing nothing.
mod revocation_gate;

/// The revocation ledger: tie breaks, compaction and the per-sender cap.
mod revocation_ledger;

/// Identity transitions: the verification rule and the rotation chain.
mod identity_rotation;

/// Identity transitions past the cap, and forged siblings of a predecessor.
mod identity_siblings;

/// Per-device cursors and the op run they read over.
mod cursors;

/// Last-writer-wins: the stamp order and what each op stamps on its row.
mod lww;

/// Fields and enum variants from a newer build survive this one.
mod unknown_values;

/// The weekly and daily reviews, their snapshots and the exports.
mod reviews;

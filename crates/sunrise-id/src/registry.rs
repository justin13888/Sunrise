//! The entity registry: the one declaration of what an entity is.
//!
//! Every entity kind is declared once, in [`crate::for_each_entity!`], with its id
//! prefix, its op-log tag, how it merges, which stream owns its ops, the
//! feature ids scoped to it, the op variants that carry it and the records
//! those ops write: each record's storage projection and every field's wire
//! name, Rust type and CRDT type ([ADR-0044] §2–§3).
//!
//! The declaration is an *x-macro*. It holds tokens, not types, and hands them
//! to a callback macro in whichever crate expands it, so each consumer derives
//! its part from the same list:
//!
//! | Consumer | Derives | Fails on a missed entity |
//! |---|---|---|
//! | `sunrise-id` (this module) | [`EntityKind`], its prefixes, [`ENTITIES`] | build |
//! | `sunrise-domain::registry` | a field-by-field check of each record type | build |
//! | `sunrise-core::inner_op` | `InnerOp`'s entity variants and their routing | build |
//! | `sunrise-core::engine::lww` | the materializer's table, key and merge class | build: its per-op match is exhaustive |
//! | `sunrise-core-bindings::dto` | that every synced record names a `UniFFI` mirror that converts from it | build |
//! | `sunrise-storage::db` | that every projected table holds its key and unknowns column | test |
//!
//! [`ENTITIES`] is the machine-readable form: the input the canonical schema
//! and its fingerprint (#323) and per-field merge (#319) are built from.
//!
//! Adding an entity is one entry here plus the code no table can write: the
//! domain type, its row writers and their materializer arm, its migration and
//! its `UniFFI` mirror. Each of those is a build or test failure until it exists.
//! `docs/02-domain/overview.md` §Adding an entity walks through it.
//!
//! # Grammar
//!
//! ```text
//! Kind {
//!     prefix: "xyz_",                         // 3 lowercase letters + `_`
//!     tag: "kind",                            // op-log `target_kind`; schema name
//!     merge: Lww | AppendOnly | Control | Unsynced,
//!     owner: Meta | Parent | Unowned | Field("stream_id"),
//!     features: ["kind.entity", ...],         // feature ids scoped here (#324)
//!     ops: [                                  // each may carry doc attributes
//!         KindCreate(Payload) = "kind.create", Create, id;
//!     ],                                      // variant(payload) = inner_kind, class, target field
//!     records: [
//!         Record @ ("table", "key", Extra | Body | Plain) | none {
//!             field: Type => Register | Map | OrSet | Counter | Nested | Derived;
//!             renamed as "wire_name": Type => Register;
//!             ..unknown                       // the flattened `Unknowns` field, if any
//!         }
//!     ],
//! }
//! ```
//!
//! The order of entries is [`EntityKind`]'s declaration order, and the order
//! of `ops` is `InnerOp`'s variant order. Neither is on the wire — CBOR
//! carries names — but both are pinned by tests.
//!
//! # Wire stability
//!
//! The prefix, the tag, every op variant name, its `inner_kind` and every
//! field's wire name are wire or storage contracts. Rename none of them.
//! [ADR-0044] §2 says a name that leaves the registry stays reserved forever,
//! but nothing records or enforces that yet. No name has left, so the grammar
//! has no reserved-name slot and no test checks for reuse. The first change
//! that removes a name adds both.
//!
//! # Not declared yet
//!
//! [ADR-0044] §2 also has the registry declare each field's **default**: the
//! value a field-level create that omits the field reads as (§4). The merge
//! (`sunrise-core`'s `engine::merge`) reads serde's defaults on the domain
//! types, and keeps its own small table for the required fields that have
//! none (`Task.title`, `state`, `stream_id`, `Context.name`). So
//! [`FieldSpec`] has no default yet. Moving that table here is a document
//! schema change: the fingerprint (#323) then hashes each default with the
//! rest of its field.
//!
//! [ADR-0044]: ../../../docs/11-adr/0044-per-field-ops.md
//! [`EntityKind`]: crate::EntityKind

/// The entity registry, handed to `$callback` as tokens.
///
/// See the [module docs](crate::registry) for the grammar and for who
/// expands it. `$callback` must be a `macro_rules!` macro in scope at the call
/// site that accepts the whole grammar.
#[macro_export]
macro_rules! for_each_entity {
    ($callback:ident) => {
        $callback! {
            /// `tsk_` — Task.
            Task {
                prefix: "tsk_",
                tag: "task",
                merge: Lww,
                owner: Field("stream_id"),
                features: [],
                ops: [
                    /// Create a task (full state).
                    TaskCreate(Task) = "task.create", Create, id;
                    /// Replace a task's full state.
                    TaskUpdate(Task) = "task.update", Update, id;
                    /// Tombstone a task, carrying the task's **full state** with `deleted`
                    /// set — not just its id.
                    ///
                    /// A delete is an op like any other in a full-state op model, and
                    /// entity-level LWW (ADR-0014) is defined as "the winning op's state
                    /// replaces the entity". An id-only delete has no state to contribute, so
                    /// when it won it set the tombstone and left every other column at
                    /// whatever the local replica happened to hold — permanently divergent
                    /// across replicas that had applied different updates, and stable, because
                    /// both then carried the same winning stamp.
                    TaskDelete(Task) = "task.delete", Delete, id;
                ],
                records: [
                    Task @ ("tasks", "id", Extra) {
                        id: EntityRef => Register;
                        created_at: Timestamp => Register;
                        updated_at: Timestamp => Register;
                        title: String => Register;
                        body: Option<NoteBody> => Register;
                        stream_id: EntityRef => Register;
                        contexts: BTreeSet<EntityRef> => OrSet;
                        state: TaskState => Register;
                        priority: Option<u8> => Register;
                        energy: Option<Energy> => Register;
                        estimated_duration_s: Option<u64> => Register;
                        scheduled_at: Option<SunriseTime> => Register;
                        due_at: Option<SunriseTime> => Register;
                        scheduling_constraints: Vec<ScheduleConstraint> => Register;
                        completed_at: Option<SunriseTime> => Register;
                        deferred_count: i64 => Counter;
                        blocks: BTreeSet<EntityRef> => Derived;
                        blocked_by: BTreeSet<EntityRef> => OrSet;
                        assignee: Option<EntityRef> => Register;
                        routine_id: Option<EntityRef> => Register;
                        routine_occurrence: Option<Timestamp> => Register;
                        reminder_lead_s: Option<u32> => Register;
                        archived: bool => Register;
                        deleted: bool => Register;
                        ..unknown
                    }
                ],
            }
            /// `str_` — Stream.
            Stream {
                prefix: "str_",
                tag: "stream",
                merge: Lww,
                owner: Meta,
                features: [],
                ops: [
                    /// Create a stream (full state).
                    StreamCreate(Stream) = "stream.create", Create, id;
                    /// Replace a stream's full state.
                    StreamUpdate(Stream) = "stream.update", Update, id;
                    /// Tombstone a stream, carrying its **full state** with `deleted` set.
                    /// See [`Self::TaskDelete`] for why an id alone does not converge.
                    StreamDelete(Stream) = "stream.delete", Delete, id;
                ],
                records: [
                    Stream @ ("streams", "stream_id", Extra) {
                        id: EntityRef => Register;
                        created_at: Timestamp => Register;
                        updated_at: Timestamp => Register;
                        name: String => Register;
                        description: Option<NoteBody> => Register;
                        color: StreamColor => Register;
                        icon: Option<String> => Register;
                        parent_id: Option<EntityRef> => Register;
                        sort_order: String => Register;
                        archived: bool => Register;
                        paused: bool => Register;
                        paused_until: Option<Timestamp> => Register;
                        review_cadence: StreamReviewCadence => Register;
                        default_context: Option<EntityRef> => Register;
                        reminder_lead_s: Option<u32> => Register;
                        deleted: bool => Register;
                        ..unknown
                    }
                ],
            }
            /// `ctx_` — Context.
            Context {
                prefix: "ctx_",
                tag: "context",
                merge: Lww,
                owner: Meta,
                features: [],
                ops: [
                    /// Create a context (full state).
                    ContextCreate(Context) = "context.create", Create, id;
                    /// Replace a context's full state.
                    ContextUpdate(Context) = "context.update", Update, id;
                    /// Tombstone a context, carrying its **full state** with `deleted` set.
                    /// See [`Self::TaskDelete`] for why an id alone does not converge.
                    ///
                    /// Every replica that applies this op also drops the context from every
                    /// Task carrying it, per `docs/02-domain/contexts-and-tags.md`.
                    ContextDelete(Context) = "context.delete", Delete, id;
                ],
                records: [
                    Context @ ("contexts", "id", Extra) {
                        id: EntityRef => Register;
                        created_at: Timestamp => Register;
                        updated_at: Timestamp => Register;
                        name: String => Register;
                        description: Option<String> => Register;
                        archived: bool => Register;
                        deleted: bool => Register;
                        ..unknown
                    }
                ],
            }
            /// `rtn_` — Routine.
            Routine {
                prefix: "rtn_",
                tag: "routine",
                merge: Lww,
                owner: Meta,
                features: [],
                ops: [
                    /// Create a routine (full state). Boxed to keep the enum small.
                    RoutineCreate(Box<Routine>) = "routine.create", Create, id;
                    /// Replace a routine's full state.
                    RoutineUpdate(Box<Routine>) = "routine.update", Update, id;
                    /// Tombstone a routine, carrying its **full state** with `deleted` set.
                    /// Boxed to keep the enum small, as create and update are. See
                    /// [`Self::TaskDelete`] for why an id alone does not converge.
                    RoutineDelete(Box<Routine>) = "routine.delete", Delete, id;
                ],
                records: [
                    Routine @ ("routines", "id", Extra) {
                        id: EntityRef => Register;
                        created_at: Timestamp => Register;
                        updated_at: Timestamp => Register;
                        template: TaskTemplate => Nested;
                        rrule: RRule => Register;
                        timezone: String => Register;
                        starts_at: Timestamp => Register;
                        ends_at: Option<Timestamp> => Register;
                        scheduling_constraints: Vec<ScheduleConstraint> => Register;
                        skip_dates: Vec<Timestamp> => Register;
                        skipped_keys: Vec<String> => OrSet;
                        catchup_policy: RoutineCatchupPolicy => Register;
                        streak_counter: i64 => Derived;
                        last_completed_at: Option<Timestamp> => Derived;
                        grace_window_s: Option<u64> => Register;
                        forgiveness_enabled: bool => Register;
                        streak_started_at: Option<Timestamp> => Derived;
                        forgivenesses_in_window: u32 => Derived;
                        streak_keys: Vec<String> => OrSet;
                        paused: bool => Register;
                        paused_until: Option<Timestamp> => Register;
                        archived: bool => Register;
                        deleted: bool => Register;
                        ..unknown
                    }
                    TaskTemplate @ none {
                        title: String => Register;
                        stream_id: EntityRef => Register;
                        contexts: Vec<EntityRef> => Register;
                        energy: Option<Energy> => Register;
                        priority: Option<u8> => Register;
                        estimated_duration_s: Option<u64> => Register;
                        body: Option<NoteBody> => Register;
                        ..unknown
                    }
                ],
            }
            /// `blk_` — Block.
            Block {
                prefix: "blk_",
                tag: "block",
                merge: Lww,
                owner: Field("stream_id"),
                features: [],
                ops: [
                    /// Create a time block (full state). Boxed to keep the enum small.
                    BlockCreate(Box<Block>) = "block.create", Create, id;
                    /// Replace a time block's full state, bindings included.
                    BlockUpdate(Box<Block>) = "block.update", Update, id;
                    /// Tombstone a time block, carrying its **full state** with `deleted` set,
                    /// bindings included. Boxed to keep the enum small, as create and update
                    /// are. See [`Self::TaskDelete`] for why an id alone does not converge.
                    BlockDelete(Box<Block>) = "block.delete", Delete, id;
                ],
                records: [
                    Block @ ("blocks", "id", Extra) {
                        id: EntityRef => Register;
                        created_at: Timestamp => Register;
                        updated_at: Timestamp => Register;
                        stream_id: EntityRef => Register;
                        starts_at: SunriseTime => Register;
                        ends_at: SunriseTime => Register;
                        title: Option<String> => Register;
                        title_track_task: bool => Register;
                        tasks: BTreeSet<EntityRef> => OrSet;
                        deleted: bool => Register;
                        ..unknown
                    }
                ],
            }
            /// `not_` — Note.
            Note {
                prefix: "not_",
                tag: "note",
                merge: Unsynced,
                owner: Unowned,
                features: [],
                ops: [],
                records: [
                    Note @ none {
                        id: EntityRef => Register;
                        created_at: Timestamp => Register;
                        updated_at: Timestamp => Register;
                        parent: EntityRef => Register;
                        body: NoteBody => Register;
                        deleted: bool => Register;
                        ..unknown
                    }
                ],
            }
            /// `att_` — Attachment.
            Attachment {
                prefix: "att_",
                tag: "attachment",
                merge: Lww,
                owner: Parent,
                features: [],
                ops: [
                    /// Record attachment metadata. Write-once: every field but the tombstone
                    /// describes one specific run of ciphertext, so there is no update op.
                    AttachmentCreate(Box<Attachment>) = "attachment.create", Create, id;
                    /// Tombstone attachment metadata, carrying its **full state** with
                    /// `deleted` set. Boxed to keep the enum small, as create is. See
                    /// [`Self::TaskDelete`] for why an id alone does not converge.
                    ///
                    /// The blob itself is reclaimed by the relay's GC after the device-cursor
                    /// quorum, not here.
                    AttachmentDelete(Box<Attachment>) = "attachment.delete", Delete, id;
                ],
                records: [
                    Attachment @ ("attachments", "id", Extra) {
                        id: EntityRef => Register;
                        created_at: Timestamp => Register;
                        updated_at: Timestamp => Register;
                        parent: EntityRef => Register;
                        filename: String => Register;
                        mime_type: String => Register;
                        size_bytes: u64 => Register;
                        blob_key: [u8; 32] => Register;
                        blob_id: [u8; 16] => Register;
                        chunk_count: u32 => Register;
                        content_hash: [u8; 32] => Register;
                        ciphertext_hash: [u8; 32] => Register;
                        deleted: bool => Register;
                        ..unknown
                    }
                ],
            }
            /// `prs_` — Person.
            Person {
                prefix: "prs_",
                tag: "person",
                merge: Unsynced,
                owner: Unowned,
                features: [],
                ops: [],
                records: [
                    Person @ none {
                        id: EntityRef => Register;
                        created_at: Timestamp => Register;
                        updated_at: Timestamp => Register;
                        display_name: String => Register;
                        identity_id: Option<EntityRef> => Register;
                        deleted: bool => Register;
                        ..unknown
                    }
                ],
            }
            /// `dev_` — Device.
            Device {
                prefix: "dev_",
                tag: "device",
                merge: Control,
                owner: Meta,
                features: [],
                ops: [],
                records: [],
            }
            /// `idn_` — Identity.
            Identity {
                prefix: "idn_",
                tag: "identity",
                merge: Control,
                owner: Meta,
                features: [],
                ops: [],
                records: [],
            }
            /// `fcs_` — Focus session (append-only; see ADR-0013).
            FocusSession {
                prefix: "fcs_",
                tag: "focus_session",
                merge: AppendOnly,
                owner: Field("stream_id"),
                features: [],
                ops: [
                    /// Open a focus session (ADR-0013's `start` op). Append-only: the record
                    /// is written once and never edited.
                    FocusStart(Box<FocusStart>) = "focus.start", Create, id;
                    /// Close a focus session (ADR-0013's `end` op). A *separate* record
                    /// addressed to the same session id — never an update of the start.
                    FocusEnd(Box<FocusEnd>) = "focus.end", Update, session_id;
                    /// Log one interruption against a session. Grow-only set semantics.
                    FocusInterrupt(Interruption) = "focus.interrupt", Update, session_id;
                ],
                records: [
                    FocusStart @ ("focus_sessions", "id", Extra) {
                        id: EntityRef => Register;
                        task_id: EntityRef => Register;
                        stream_id: EntityRef => Register;
                        started_at as "started_at_ms": Timestamp => Register;
                        planned_ms: Option<u64> => Register;
                        energy: Option<Energy> => Register;
                        kind: FocusKind => Register;
                        chunk: Option<Chunk> => Register;
                        ..unknown
                    }
                    FocusEnd @ ("focus_session_ends", "session_id", Extra) {
                        session_id: EntityRef => Register;
                        ended_at as "ended_at_ms": Timestamp => Register;
                        actual_focused_ms: u64 => Register;
                        interruptions: Vec<Interruption> => Register;
                        completed_task: bool => Register;
                        ..unknown
                    }
                    Interruption @ ("focus_interruptions", "session_id", Plain) {
                        session_id: EntityRef => Register;
                        at as "at_ms": Timestamp => Register;
                        reason: InterruptionReason => Register;
                    }
                ],
            }
            /// `rvw_` — Saved review snapshot (append-only; see
            /// `docs/08-features/reviews-and-stats.md` §Weekly review step 5).
            ReviewSnapshot {
                prefix: "rvw_",
                tag: "review_snapshot",
                merge: AppendOnly,
                owner: Meta,
                features: [],
                ops: [
                    /// Record a completed weekly review (`docs/08-features/reviews-and-stats.md`
                    /// §Weekly review step 5). Append-only, exactly like the focus family: the
                    /// snapshot is written once under its own `rvw_` id and never edited, so
                    /// two devices reviewing the same week produce two records rather than a
                    /// lost write.
                    ReviewSnapshotCreate(Box<ReviewSnapshot>) = "review.snapshot", Create, id;
                ],
                records: [
                    ReviewSnapshot @ ("review_snapshots", "id", Body) {
                        id: EntityRef => Register;
                        created_at: Timestamp => Register;
                        window_start as "window_start_ms": Timestamp => Register;
                        window_end as "window_end_ms": Timestamp => Register;
                        totals: ReviewTotals => Register;
                        streams: Vec<ReviewSnapshotStream> => Register;
                        streaks: Vec<StreakRow> => Register;
                        note: Option<String> => Register;
                        ..unknown
                    }
                ],
            }
        }
    };
}

/// How an entity's ops merge into its materialized state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Merge {
    /// Every field merges by its own CRDT type (ADR-0044): a full-state op is
    /// a write to every field it carries, and a `Patch` writes the fields it
    /// names.
    Lww,
    /// Each op writes one immutable record under its own key and never
    /// contends with another (ADR-0013).
    AppendOnly,
    /// Carried only by control ops, which hold key material or trust and
    /// never reach the entity materializer (ADR-0024, ADR-0032).
    Control,
    /// No op family carries it yet: the type exists, nothing syncs it.
    Unsynced,
}

/// Which stream's key seals an entity's ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// The vault-meta stream.
    Meta,
    /// The stream the named field of the entity's first record holds.
    Field(&'static str),
    /// The stream of the entity its `parent` field names.
    Parent,
    /// No op is ever sealed for it ([`Merge::Unsynced`]).
    Unowned,
}

/// An op's effect on its entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpClass {
    /// Brings the entity into being.
    Create,
    /// Changes it, or adds a record to it.
    Update,
    /// Tombstones it.
    Delete,
}

/// How a field merges ([ADR-0044] §3).
///
/// [ADR-0044]: ../../../docs/11-adr/0044-per-field-ops.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crdt {
    /// One last-writer-wins value: scalars, optionals, and nested values
    /// written as a unit.
    Register,
    /// One register per key.
    Map,
    /// Add-wins observed-remove set.
    OrSet,
    /// PN-counter.
    Counter,
    /// A nested record whose own fields each merge by their declared type.
    Nested,
    /// Computed at read time from other fields; not a field of the merge.
    Derived,
}

/// Where a record keeps the fields this build does not know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unknowns {
    /// In an `extra` CBOR blob column beside the projected columns.
    Extra,
    /// Inside the row's whole-record CBOR `body` column.
    Body,
    /// Nowhere: the record has no unknowns map.
    Plain,
}

/// One registered entity kind.
#[derive(Debug, Clone, Copy)]
pub struct EntitySpec {
    /// The kind.
    pub kind: EntityKind,
    /// The id prefix.
    pub prefix: &'static str,
    /// The op-log `target_kind` and the entity's schema name.
    pub tag: &'static str,
    /// How its ops merge.
    pub merge: Merge,
    /// Which stream seals its ops.
    pub owner: Owner,
    /// The feature ids scoped to it (ADR-0045 §7).
    pub features: &'static [&'static str],
    /// The op variants that carry it, in `InnerOp` order.
    pub ops: &'static [OpSpec],
    /// The records its ops write. The first is the entity's own.
    pub records: &'static [RecordSpec],
}

/// One op variant.
#[derive(Debug, Clone, Copy)]
pub struct OpSpec {
    /// The `InnerOp` variant name, which is the CBOR tag on the wire.
    pub variant: &'static str,
    /// The payload type, as written in the registry.
    pub payload: &'static str,
    /// The op-log `inner_kind`.
    pub inner_kind: &'static str,
    /// The op's effect.
    pub class: OpClass,
    /// The payload field naming the target entity.
    pub target: &'static str,
}

/// One record type an entity's ops write.
#[derive(Debug, Clone, Copy)]
pub struct RecordSpec {
    /// The Rust type name in `sunrise-domain`.
    pub name: &'static str,
    /// Its materialized table, or `None` when it is not projected on its own.
    pub storage: Option<Storage>,
    /// Its fields, in declaration order.
    pub fields: &'static [FieldSpec],
    /// The Rust name of its flattened unknowns field, if it has one.
    pub unknowns: Option<&'static str>,
}

/// A record's storage projection.
#[derive(Debug, Clone, Copy)]
pub struct Storage {
    /// The table.
    pub table: &'static str,
    /// The column holding the entity id.
    pub key: &'static str,
    /// Where the row keeps unknown fields.
    pub unknowns: Unknowns,
}

/// One field of a record.
///
/// It has no default yet. See the [module docs](crate::registry) §Not declared
/// yet.
#[derive(Debug, Clone, Copy)]
pub struct FieldSpec {
    /// The CBOR map key, which is the field's identity ([ADR-0044] §2).
    ///
    /// [ADR-0044]: ../../../docs/11-adr/0044-per-field-ops.md
    pub name: &'static str,
    /// The Rust field name.
    pub rust: &'static str,
    /// The value type, as written in the registry.
    pub value_type: &'static str,
    /// How it merges.
    pub crdt: Crdt,
}

/// One value type: a type a record field or an op payload carries that is not
/// itself a registered record, described by [`describe_value_types!`].
///
/// [`describe_value_types!`]: crate::describe_value_types
#[derive(Debug, Clone, Copy)]
pub struct ValueSpec {
    /// The Rust type name.
    pub name: &'static str,
    /// Its wire shape.
    pub shape: ValueShape,
}

/// How a value type is laid out on the wire.
#[derive(Debug, Clone, Copy)]
pub enum ValueShape {
    /// A map of named fields.
    Record {
        /// Its fields, in declaration order.
        fields: &'static [ValueField],
        /// Whether it keeps the fields this build does not know.
        keeps_unknowns: bool,
    },
    /// Encoded exactly as the type it names: a transparent newtype, or a type
    /// whose hand-written serde writes that type.
    Alias(&'static str),
    /// A fixed-length array of positional items, in order.
    Tuple(&'static [&'static str]),
    /// One of a closed set of variants.
    Variants {
        /// Where the variant's name goes.
        tagging: Tagging,
        /// The variants, in declaration order.
        variants: &'static [VariantSpec],
        /// Whether an unknown variant is kept rather than refused.
        keeps_unknowns: bool,
    },
}

/// Where a [`ValueShape::Variants`] value writes its variant's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tagging {
    /// serde's external tagging: a unit variant is its name as a string, any
    /// other is a one-entry map from its name to its content.
    External,
    /// One map whose named key holds the variant's name beside its fields.
    Internal(&'static str),
}

/// One variant of a [`ValueShape::Variants`] type.
#[derive(Debug, Clone, Copy)]
pub struct VariantSpec {
    /// The name on the wire.
    pub name: &'static str,
    /// The Rust variant name.
    pub rust: &'static str,
    /// What it carries.
    pub shape: VariantShape,
}

/// What one variant carries.
#[derive(Debug, Clone, Copy)]
pub enum VariantShape {
    /// Nothing.
    Unit,
    /// One value of the named type.
    Newtype(&'static str),
    /// Named fields.
    Fields(&'static [ValueField]),
}

/// One field of a value type. It has no CRDT type: a value type is written as
/// a unit inside the field that holds it.
#[derive(Debug, Clone, Copy)]
pub struct ValueField {
    /// The CBOR map key.
    pub name: &'static str,
    /// The Rust field name.
    pub rust: &'static str,
    /// The value type, as written in the declaration.
    pub value_type: &'static str,
}

impl EntitySpec {
    /// The materialized table the entity's merge stamp lives in: its first
    /// record's.
    #[must_use]
    pub fn storage(&self) -> Option<Storage> {
        self.records.first().and_then(|r| r.storage)
    }

    /// The registered record named `name`.
    #[must_use]
    pub fn record(&self, name: &str) -> Option<&'static RecordSpec> {
        self.records.iter().find(|r| r.name == name)
    }
}

#[doc(hidden)]
#[macro_export]
macro_rules! __registry_storage {
    (none) => {
        ::core::option::Option::None
    };
    (($table:literal, $key:literal, $unknowns:ident)) => {
        ::core::option::Option::Some($crate::registry::Storage {
            table: $table,
            key: $key,
            unknowns: $crate::registry::Unknowns::$unknowns,
        })
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __registry_wire_name {
    ($field:ident) => {
        ::core::stringify!($field)
    };
    ($field:ident $wire:literal) => {
        $wire
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __registry_option_name {
    () => {
        ::core::option::Option::None
    };
    ($name:ident) => {
        ::core::option::Option::Some(::core::stringify!($name))
    };
}

/// Describe the value types a crate puts on the wire, and check every
/// description against the type it names.
///
/// Expands to a `static` slice of [`ValueSpec`] and, beside it, one function
/// per type that the compiler checks and nothing calls:
///
/// | Section | Wire shape | Fails the build when |
/// |---|---|---|
/// | `records` | a map of named fields | a field is added, removed, or retyped (an exhaustive destructuring with no `..`) |
/// | `newtypes` | its one field, transparently | the field's type changes (the constructor coerces to `fn(T) -> Self`) |
/// | `aliases` | the type named | never: its serde is hand-written, so a wire test pins it |
/// | `tuples` | an array of its fields, in order | an item is added, removed, or retyped (the constructor coerces) |
/// | `variants` | one of its variants | a variant is added or removed (an exhaustive `match`), or a variant's fields or payload change |
///
/// ```text
/// describe_value_types! {
///     pub static NAME;
///     records: [ Rec { field: Type; renamed as "wire": Type; ..unknown } ],
///     newtypes: [ Body(Vec<u8>); ],
///     aliases: [ Set = Vec<Day>; ],
///     tuples: [ Entry([u8; 16], u64); ],
///     variants: [
///         Time "kind" {                      // internally tagged; or `external`
///             Instant as "instant" = { at: Timestamp; };
///             ..Unknown                      // an arm that keeps an unknown variant
///         }
///         Who external { Device = ([u8; 16]); Nobody = unit; }
///     ],
/// }
/// ```
///
/// Every type named must be in scope at the call site, and so must
/// `Unknowns` where a record keeps unknowns. Wire names are checked against
/// what serde writes by each caller's tests, as the entity registry's are.
#[macro_export]
macro_rules! describe_value_types {
    (
        $(#[$meta:meta])*
        $vis:vis static $name:ident;
        records: [
            $(
                $record:ident {
                    $( $field:ident $(as $wire:literal)?: $field_ty:ty; )*
                    $(..$unknown:ident)?
                }
            )*
        ],
        newtypes: [ $( $newtype:ident($newtype_inner:ty); )* ],
        aliases: [ $( $alias:ident = $alias_ty:ty; )* ],
        tuples: [ $( $tuple:ident( $($item:ty),* $(,)? ); )* ],
        variants: [
            $(
                $enum_ty:ident $tagging:tt {
                    $( $variant:ident $(as $variant_wire:literal)? = $variant_body:tt; )*
                    $(..$unknown_variant:ident)?
                }
            )*
        ],
    ) => {
        $(#[$meta])*
        $vis static $name: &[$crate::registry::ValueSpec] = &[
            $(
                $crate::registry::ValueSpec {
                    name: ::core::stringify!($record),
                    shape: $crate::registry::ValueShape::Record {
                        fields: &[
                            $(
                                $crate::registry::ValueField {
                                    name: $crate::__registry_wire_name!($field $($wire)?),
                                    rust: ::core::stringify!($field),
                                    value_type: ::core::stringify!($field_ty),
                                },
                            )*
                        ],
                        keeps_unknowns: $crate::__registry_present!($($unknown)?),
                    },
                },
            )*
            $(
                $crate::registry::ValueSpec {
                    name: ::core::stringify!($newtype),
                    shape: $crate::registry::ValueShape::Alias(::core::stringify!($newtype_inner)),
                },
            )*
            $(
                $crate::registry::ValueSpec {
                    name: ::core::stringify!($alias),
                    shape: $crate::registry::ValueShape::Alias(::core::stringify!($alias_ty)),
                },
            )*
            $(
                $crate::registry::ValueSpec {
                    name: ::core::stringify!($tuple),
                    shape: $crate::registry::ValueShape::Tuple(&[$(::core::stringify!($item)),*]),
                },
            )*
            $(
                $crate::registry::ValueSpec {
                    name: ::core::stringify!($enum_ty),
                    shape: $crate::registry::ValueShape::Variants {
                        tagging: $crate::__value_tagging!($tagging),
                        variants: &[
                            $(
                                $crate::registry::VariantSpec {
                                    name: $crate::__registry_wire_name!($variant $($variant_wire)?),
                                    rust: ::core::stringify!($variant),
                                    shape: $crate::__value_variant_shape!($variant_body),
                                },
                            )*
                        ],
                        keeps_unknowns: $crate::__registry_present!($($unknown_variant)?),
                    },
                },
            )*
        ];

        $(
            const _: () = {
                #[allow(dead_code, clippy::no_effect_underscore_binding)]
                fn described_fields_are_the_type_fields(value: &$record) {
                    let $record { $($field,)* $($unknown,)? } = value;
                    $( let _: &$field_ty = $field; )*
                    $( let _: &Unknowns = $unknown; )?
                }
            };
        )*
        $(
            const _: () = {
                #[allow(dead_code)]
                fn described_newtype_is_the_type() {
                    let _: fn($newtype_inner) -> $newtype = $newtype;
                }
            };
        )*
        $(
            const _: () = {
                #[allow(dead_code)]
                fn described_items_are_the_type_items() {
                    let _: fn($($item),*) -> $tuple = $tuple;
                }
            };
        )*
        $(
            const _: () = {
                #[allow(dead_code)]
                fn described_variants_are_the_type_variants(value: &$enum_ty) {
                    match value {
                        $( $enum_ty::$variant { .. } => {} )*
                        $( $enum_ty::$unknown_variant { .. } => {} )?
                    }
                    $( $crate::__value_variant_check!(value, $enum_ty, $variant, $variant_body); )*
                }
            };
        )*
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __registry_present {
    () => {
        false
    };
    ($name:ident) => {
        true
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __value_tagging {
    (external) => {
        $crate::registry::Tagging::External
    };
    ($tag:literal) => {
        $crate::registry::Tagging::Internal($tag)
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __value_variant_shape {
    (unit) => {
        $crate::registry::VariantShape::Unit
    };
    (($ty:ty)) => {
        $crate::registry::VariantShape::Newtype(::core::stringify!($ty))
    };
    ({ $( $field:ident $(as $wire:literal)?: $field_ty:ty; )* }) => {
        $crate::registry::VariantShape::Fields(&[
            $(
                $crate::registry::ValueField {
                    name: $crate::__registry_wire_name!($field $($wire)?),
                    rust: ::core::stringify!($field),
                    value_type: ::core::stringify!($field_ty),
                },
            )*
        ])
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __value_variant_check {
    ($value:ident, $enum_ty:ident, $variant:ident, unit) => {
        let _: $enum_ty = $enum_ty::$variant;
    };
    ($value:ident, $enum_ty:ident, $variant:ident, ($ty:ty)) => {
        let _: fn($ty) -> $enum_ty = $enum_ty::$variant;
    };
    ($value:ident, $enum_ty:ident, $variant:ident, { $( $field:ident $(as $wire:literal)?: $field_ty:ty; )* }) => {
        if let $enum_ty::$variant { $($field,)* } = $value {
            $( let _: &$field_ty = $field; )*
        }
    };
}

/// Expands the registry into [`EntityKind`] and [`ENTITIES`].
macro_rules! define_entities {
    (
        $(
            $(#[$kind_meta:meta])*
            $kind:ident {
                prefix: $prefix:literal,
                tag: $tag:literal,
                merge: $merge:ident,
                owner: $owner:ident $(($owner_field:literal))?,
                features: [$($feature:literal),* $(,)?],
                ops: [
                    $(
                        $(#[$op_meta:meta])*
                        $op:ident($payload:ty) = $inner_kind:literal, $class:ident, $target:ident;
                    )*
                ],
                records: [
                    $(
                        $record:ident @ $storage:tt {
                            $(
                                $field:ident $(as $wire:literal)?: $field_ty:ty => $crdt:ident;
                            )*
                            $(..$unknown:ident)?
                        }
                    )*
                ],
            }
        )*
    ) => {
        /// Domain entity kinds, generated from [`for_each_entity!`](crate::for_each_entity).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
        #[serde(rename_all = "lowercase")]
        #[non_exhaustive]
        pub enum EntityKind {
            $( $(#[$kind_meta])* $kind, )*
        }

        impl EntityKind {
            /// How many kinds the registry declares.
            pub const COUNT: usize = [$(::core::stringify!($kind)),*].len();

            /// Canonical 4-char prefix (3 letters + `_`).
            #[must_use]
            pub const fn prefix(self) -> &'static str {
                match self {
                    $( Self::$kind => $prefix, )*
                }
            }

            /// Look up an entity kind from its 4-char prefix (including the `_`).
            /// Returns `None` for unknown prefixes.
            #[must_use]
            pub fn from_prefix(prefix: &str) -> Option<Self> {
                match prefix {
                    $( $prefix => Some(Self::$kind), )*
                    _ => None,
                }
            }

            /// The op-log `target_kind` and schema name, e.g. `"focus_session"`.
            #[must_use]
            pub const fn tag(self) -> &'static str {
                match self {
                    $( Self::$kind => $tag, )*
                }
            }

            /// All kinds, in declaration order.
            #[must_use]
            pub const fn all() -> [Self; Self::COUNT] {
                [ $( Self::$kind, )* ]
            }

            /// This kind's registry entry.
            #[must_use]
            pub fn spec(self) -> &'static $crate::registry::EntitySpec {
                &$crate::registry::ENTITIES[self as usize]
            }
        }

        /// Every registered entity, in [`EntityKind`] declaration order.
        pub static ENTITIES: [EntitySpec; EntityKind::COUNT] = [
            $(
                EntitySpec {
                    kind: EntityKind::$kind,
                    prefix: $prefix,
                    tag: $tag,
                    merge: Merge::$merge,
                    owner: Owner::$owner $(($owner_field))?,
                    features: &[$($feature),*],
                    ops: &[
                        $(
                            OpSpec {
                                variant: ::core::stringify!($op),
                                payload: ::core::stringify!($payload),
                                inner_kind: $inner_kind,
                                class: OpClass::$class,
                                target: ::core::stringify!($target),
                            },
                        )*
                    ],
                    records: &[
                        $(
                            RecordSpec {
                                name: ::core::stringify!($record),
                                storage: $crate::__registry_storage!($storage),
                                fields: &[
                                    $(
                                        FieldSpec {
                                            name: $crate::__registry_wire_name!($field $($wire)?),
                                            rust: ::core::stringify!($field),
                                            value_type: ::core::stringify!($field_ty),
                                            crdt: Crdt::$crdt,
                                        },
                                    )*
                                ],
                                unknowns: $crate::__registry_option_name!($($unknown)?),
                            },
                        )*
                    ],
                },
            )*
        ];
    };
}

crate::for_each_entity!(define_entities);

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn entries_are_in_entity_kind_order() {
        assert_eq!(ENTITIES.len(), EntityKind::COUNT);
        for (spec, kind) in ENTITIES.iter().zip(EntityKind::all()) {
            assert_eq!(spec.kind, kind);
            assert!(std::ptr::eq(kind.spec(), spec));
            assert_eq!(spec.prefix, kind.prefix());
            assert_eq!(spec.tag, kind.tag());
        }
    }

    #[test]
    fn names_are_unique_where_the_wire_needs_them_to_be() {
        let unique = |names: Vec<&str>, what: &str| {
            let set: BTreeSet<&str> = names.iter().copied().collect();
            assert_eq!(set.len(), names.len(), "duplicate {what}: {names:?}");
        };
        unique(ENTITIES.iter().map(|e| e.tag).collect(), "tag");
        let ops = || ENTITIES.iter().flat_map(|e| e.ops.iter());
        unique(ops().map(|o| o.variant).collect(), "op variant");
        unique(ops().map(|o| o.inner_kind).collect(), "inner_kind");
        for record in ENTITIES.iter().flat_map(|e| e.records.iter()) {
            unique(record.fields.iter().map(|f| f.name).collect(), "field name");
            unique(record.fields.iter().map(|f| f.rust).collect(), "rust field");
        }
    }

    /// The merge class decides what the rest of an entry must hold, and a
    /// half-declared entity is exactly what this registry exists to stop.
    #[test]
    fn every_entry_is_complete_for_its_merge_class() {
        for e in &ENTITIES {
            match e.merge {
                Merge::Lww | Merge::AppendOnly => {
                    assert!(!e.ops.is_empty(), "{:?} syncs but has no op", e.kind);
                    assert!(
                        e.storage().is_some(),
                        "{:?} syncs but its first record is not projected",
                        e.kind
                    );
                    assert_eq!(
                        e.ops[0].class,
                        OpClass::Create,
                        "{:?}'s first op must create it",
                        e.kind
                    );
                    assert_ne!(e.owner, Owner::Unowned, "{:?} has ops", e.kind);
                }
                Merge::Control | Merge::Unsynced => {
                    assert!(e.ops.is_empty(), "{:?} has no entity ops", e.kind);
                }
            }
            if e.merge == Merge::AppendOnly {
                assert!(
                    e.ops.iter().all(|o| o.class != OpClass::Delete),
                    "{:?} is append-only and cannot be deleted",
                    e.kind
                );
            }
            if e.merge == Merge::Unsynced {
                assert_eq!(e.owner, Owner::Unowned);
            }
        }
    }

    #[test]
    fn op_kinds_and_targets_name_their_entity() {
        for e in &ENTITIES {
            for op in e.ops {
                assert!(
                    op.variant.starts_with(match e.kind {
                        EntityKind::FocusSession => "Focus",
                        _ => e.records[0].name,
                    }),
                    "{} does not name {:?}",
                    op.variant,
                    e.kind
                );
                let family = op.inner_kind.split('.').next().unwrap_or_default();
                assert!(
                    e.tag.starts_with(family),
                    "{} is not in {}'s family",
                    op.inner_kind,
                    e.tag
                );
                assert!(
                    e.records
                        .iter()
                        .any(|r| r.fields.iter().any(|f| f.rust == op.target)),
                    "{}'s target `{}` is not a registered field",
                    op.variant,
                    op.target
                );
            }
        }
    }

    #[test]
    fn owner_fields_and_storage_agree_with_the_records() {
        for e in &ENTITIES {
            match e.owner {
                Owner::Field(name) => assert!(
                    e.records[0].fields.iter().any(|f| f.rust == name),
                    "{:?}'s owner field `{name}` is not on its first record",
                    e.kind
                ),
                Owner::Parent => assert!(
                    e.records[0].fields.iter().any(|f| f.rust == "parent"),
                    "{:?} is owned by a parent it does not name",
                    e.kind
                ),
                Owner::Meta | Owner::Unowned => {}
            }
            for r in e.records {
                if let Some(s) = r.storage {
                    assert_eq!(
                        s.unknowns != Unknowns::Plain,
                        r.unknowns.is_some(),
                        "{}: a record with an unknowns map needs a column for it, and only then",
                        r.name
                    );
                    assert!(
                        r.fields
                            .iter()
                            .any(|f| f.rust == "id" || f.rust == "session_id"),
                        "{} is keyed on a field it does not declare",
                        r.name
                    );
                }
                for f in r.fields {
                    if f.crdt == Crdt::Nested {
                        assert!(
                            e.records.iter().any(|n| n.name == f.value_type),
                            "{}.{} nests an unregistered record",
                            r.name,
                            f.rust
                        );
                    }
                }
            }
        }
    }
}

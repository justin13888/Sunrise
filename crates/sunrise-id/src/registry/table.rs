//! The registry's data: [`crate::for_each_entity!`], every entity kind's
//! declaration, in the grammar the [module docs](crate::registry) give.

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
                    /// A full-state op writes every field it carries (ADR-0044 §7), so an
                    /// id-only delete would write none (ADR-0014): when it won it set the
                    /// tombstone and left every other column at whatever the local replica
                    /// happened to hold — permanently divergent across replicas that had
                    /// applied different updates, and stable, because both then carried the
                    /// same winning stamp.
                    TaskDelete(Task) = "task.delete", Delete, id;
                ],
                records: [
                    Task @ ("tasks", "id", Extra) {
                        id: EntityRef => Register(Quiet);
                        created_at: Timestamp => Register(Quiet);
                        updated_at: Timestamp => Register(Quiet);
                        title: String => Register(Tracked) = "\"\"";
                        body: Option<NoteBody> => Register(Tracked);
                        // No stream (ADR-0046), spelled as the legacy Inbox
                        // reference (`sunrise_domain::INBOX_STREAM_ID`).
                        stream_id: Option<EntityRef> => Register(Tracked) = "\"str_0000076XBEE9MQ6S9ED5Q64VVR\"";
                        contexts: BTreeSet<EntityRef> => OrSet;
                        state: TaskState => Register(State) = "\"todo\"";
                        priority: Option<u8> => Register(Quiet);
                        energy: Option<Energy> => Register(Quiet);
                        estimated_duration_s: Option<u64> => Register(Quiet);
                        // ADR-0047 §1: the plan, and the hard deadline. They keep
                        // the wire names every build reads with their meaning
                        // (ADR-0047 C1), and the soft deadline is new.
                        planned_at as "scheduled_at": Option<SunriseTime> => Register(Tracked);
                        target_at: Option<SunriseTime> => Register(Tracked);
                        hard_due_at as "due_at": Option<SunriseTime> => Register(Tracked);
                        scheduling_constraints: Vec<ScheduleConstraint> => Register(Tracked);
                        completed_at: Option<SunriseTime> => Register(Quiet);
                        deferred_count: i64 => Counter;
                        blocks: BTreeSet<EntityRef> => Derived;
                        blocked_by: BTreeSet<EntityRef> => OrSet;
                        assignee: Option<EntityRef> => Register(Tracked);
                        routine_id: Option<EntityRef> => Register(Quiet);
                        routine_occurrence: Option<Timestamp> => Register(Quiet);
                        // #331: set by the routine engine when it generates an
                        // occurrence that breaks a hard constraint.
                        hard_constraint_violated: bool => Register(Quiet);
                        reminder_lead_s: Option<u32> => Register(Quiet);
                        sort_order: Option<String> => Register(Quiet);
                        triaged_at: Option<Timestamp> => Register(Quiet);
                        // ADR-0047 §4: the drop reason and the triage Keep.
                        cancel_reason: Option<String> => Register(Quiet);
                        late_acknowledged_at: Option<Timestamp> => Register(Quiet);
                        archived: bool => Register(Quiet);
                        deleted: bool => Register(Tombstone);
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
                        id: EntityRef => Register(Quiet);
                        created_at: Timestamp => Register(Quiet);
                        updated_at: Timestamp => Register(Quiet);
                        name: String => Register(Tracked);
                        description: Option<NoteBody> => Register(Tracked);
                        color: StreamColor => Register(Quiet);
                        icon: Option<String> => Register(Quiet);
                        parent_id: Option<EntityRef> => Register(Tracked);
                        sort_order: String => Register(Quiet);
                        archived: bool => Register(Quiet);
                        paused: bool => Register(Quiet);
                        paused_until: Option<CompatTime> => Register(Tracked);
                        review_cadence: StreamReviewCadence => Register(Quiet);
                        default_context: Option<EntityRef> => Register(Quiet);
                        reminder_lead_s: Option<u32> => Register(Quiet);
                        deleted: bool => Register(Tombstone);
                        rehomed_to: Option<EntityRef> => Register(Tracked);
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
                        id: EntityRef => Register(Quiet);
                        created_at: Timestamp => Register(Quiet);
                        updated_at: Timestamp => Register(Quiet);
                        name: String => Register(Tracked) = "\"\"";
                        description: Option<String> => Register(Tracked);
                        archived: bool => Register(Quiet);
                        deleted: bool => Register(Tombstone);
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
                features: ["routine.civil_times", "routine.series"],
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
                        id: EntityRef => Register(Quiet);
                        created_at: Timestamp => Register(Quiet);
                        updated_at: Timestamp => Register(Quiet);
                        template: TaskTemplate => Nested;
                        rrule: RRule => Register(Tracked);
                        timezone: String => Register(Tracked);
                        starts_at: CompatTime => Register(Tracked);
                        ends_at: Option<CompatTime> => Register(Tracked);
                        scheduling_constraints: Vec<ScheduleConstraint> => Register(Tracked);
                        skip_dates: Vec<Timestamp> => Register(Tracked);
                        skipped_keys: Vec<String> => OrSet;
                        catchup_policy: RoutineCatchupPolicy => Register(Quiet);
                        streak_counter: i64 => Derived;
                        last_completed_at: Option<CompatTime> => Derived;
                        grace_window_s: Option<u64> => Register(Quiet);
                        forgiveness_enabled: bool => Register(Quiet);
                        streak_started_at: Option<Timestamp> => Derived;
                        forgivenesses_in_window: u32 => Derived;
                        streak_keys: Vec<String> => OrSet;
                        paused: bool => Register(Quiet);
                        paused_until: Option<CompatTime> => Register(Tracked);
                        archived: bool => Register(Quiet);
                        deleted: bool => Register(Tombstone);
                        // #331: the series root a split routine continues,
                        // written once, at the split.
                        split_from: Option<EntityRef> => Register(Quiet);
                        ..unknown
                    }
                    TaskTemplate @ none {
                        title: String => Register(Tracked);
                        stream_id: Option<EntityRef> => Register(Tracked);
                        contexts: Vec<EntityRef> => Register(Tracked);
                        energy: Option<Energy> => Register(Tracked);
                        priority: Option<u8> => Register(Tracked);
                        estimated_duration_s: Option<u64> => Register(Tracked);
                        body: Option<NoteBody> => Register(Tracked);
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
                        id: EntityRef => Register(Quiet);
                        created_at: Timestamp => Register(Quiet);
                        updated_at: Timestamp => Register(Quiet);
                        stream_id: Option<EntityRef> => Register(Tracked);
                        starts_at: SunriseTime => Register(Tracked);
                        ends_at: SunriseTime => Register(Tracked);
                        title: Option<String> => Register(Tracked);
                        title_track_task: bool => Register(Quiet);
                        tasks: BTreeSet<EntityRef> => OrSet;
                        deleted: bool => Register(Tombstone);
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
                        id: EntityRef => Register(Quiet);
                        created_at: Timestamp => Register(Quiet);
                        updated_at: Timestamp => Register(Quiet);
                        parent: EntityRef => Register(Quiet);
                        body: NoteBody => Register(Quiet);
                        deleted: bool => Register(Quiet);
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
                features: ["attachment.thumbnail"],
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
                        id: EntityRef => Register(Quiet);
                        created_at: Timestamp => Register(Quiet);
                        updated_at: Timestamp => Register(Quiet);
                        parent: EntityRef => Register(Quiet);
                        filename: String => Register(Quiet);
                        mime_type: String => Register(Quiet);
                        size_bytes: u64 => Register(Quiet);
                        blob_key: [u8; 32] => Register(Quiet);
                        blob_id: [u8; 16] => Register(Quiet);
                        chunk_count: u32 => Register(Quiet);
                        content_hash: [u8; 32] => Register(Quiet);
                        ciphertext_hash: [u8; 32] => Register(Quiet);
                        width: Option<u32> => Register(Quiet);
                        height: Option<u32> => Register(Quiet);
                        thumbnail_blob_id: Option<[u8; 16]> => Register(Quiet);
                        thumbnail_blob_key: Option<[u8; 32]> => Register(Quiet);
                        thumbnail_mime: Option<String> => Register(Quiet);
                        thumbnail_size_bytes: Option<u32> => Register(Quiet);
                        thumbnail_content_hash: Option<[u8; 32]> => Register(Quiet);
                        thumbnail_ciphertext_hash: Option<[u8; 32]> => Register(Quiet);
                        deleted: bool => Register(Tombstone);
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
                        id: EntityRef => Register(Quiet);
                        created_at: Timestamp => Register(Quiet);
                        updated_at: Timestamp => Register(Quiet);
                        display_name: String => Register(Quiet);
                        identity_id: Option<EntityRef> => Register(Quiet);
                        deleted: bool => Register(Quiet);
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
                        id: EntityRef => Register(Quiet);
                        task_id: EntityRef => Register(Quiet);
                        stream_id: EntityRef => Register(Quiet);
                        started_at as "started_at_ms": Timestamp => Register(Quiet);
                        planned_ms: Option<u64> => Register(Quiet);
                        energy: Option<Energy> => Register(Quiet);
                        kind: FocusKind => Register(Quiet);
                        chunk: Option<Chunk> => Register(Quiet);
                        ..unknown
                    }
                    FocusEnd @ ("focus_session_ends", "session_id", Extra) {
                        session_id: EntityRef => Register(Quiet);
                        ended_at as "ended_at_ms": Timestamp => Register(Quiet);
                        actual_focused_ms: u64 => Register(Quiet);
                        interruptions: Vec<Interruption> => Register(Quiet);
                        completed_task: bool => Register(Quiet);
                        ..unknown
                    }
                    Interruption @ ("focus_interruptions", "session_id", Plain) {
                        session_id: EntityRef => Register(Quiet);
                        at as "at_ms": Timestamp => Register(Quiet);
                        reason: InterruptionReason => Register(Quiet);
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
                        id: EntityRef => Register(Quiet);
                        created_at: Timestamp => Register(Quiet);
                        window_start as "window_start_ms": Timestamp => Register(Quiet);
                        window_end as "window_end_ms": Timestamp => Register(Quiet);
                        totals: ReviewTotals => Register(Quiet);
                        streams: Vec<ReviewSnapshotStream> => Register(Quiet);
                        streaks: Vec<StreakRow> => Register(Quiet);
                        note: Option<String> => Register(Quiet);
                        ..unknown
                    }
                ],
            }
            /// `prf_` — the vault's Preferences, one per vault under the zero
            /// id (ADR-0050, `docs/02-domain/preferences.md`).
            ///
            /// Written only by `Patch`: it has no full-state op family, so no
            /// build can overwrite it whole, and `ops` is empty.
            Preferences {
                prefix: "prf_",
                tag: "preferences",
                merge: Lww,
                owner: Meta,
                features: ["preferences.entity"],
                ops: [],
                records: [
                    Preferences @ ("preferences", "id", Extra) {
                        id: EntityRef => Register(Quiet);
                        created_at: Timestamp => Register(Quiet);
                        updated_at: Timestamp => Register(Quiet);
                        values: BTreeMap<String, CborValue> => Map;
                        ..unknown
                    }
                ],
            }
            /// `svw_` — a saved view (issue #341,
            /// `docs/08-features/planning-views.md` §Saved views).
            ///
            /// Written only by `Patch`, as Preferences is: it has no
            /// full-state op family, so `ops` is empty, and each field is its
            /// own register, so concurrent edits of different fields survive.
            SavedView {
                prefix: "svw_",
                tag: "saved_view",
                merge: Lww,
                owner: Meta,
                features: ["saved_view.entity"],
                ops: [],
                records: [
                    SavedView @ ("saved_views", "id", Extra) {
                        id: EntityRef => Register(Quiet);
                        created_at: Timestamp => Register(Quiet);
                        updated_at: Timestamp => Register(Quiet);
                        name: String => Register(Tracked);
                        view: ViewKind => Register(Tracked);
                        filter: ViewFilter => Register(Tracked);
                        sort: ViewSort => Register(Quiet);
                        sort_order: Option<String> => Register(Quiet);
                        deleted: bool => Register(Tombstone);
                        ..unknown
                    }
                ],
            }
        }
    };
}

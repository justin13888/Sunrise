//! Rebuilding the projection from the local op log (issue #441, ADR-0065
//! PR 2): what it re-derives, what it keeps, and what a compacted log hands
//! it.

use super::testutil::*;
use super::*;
use crate::engine::rebuild::check::{dump, projection_tables};
use crate::engine::rebuild::{TableClass, TABLE_CLASSES};
use crate::engine::{CompactionPolicy, RebuildReport};
use sunrise_crypto::decode_envelope;
use sunrise_domain::{PrefTarget, PrefValue, INBOX_STREAM_BYTES};

const INBOX: [u8; 16] = INBOX_STREAM_BYTES;

const DAY: u64 = 24 * 60 * 60 * 1000;

/// Two replicas of one account that trust each other, on one clock, each
/// having advertised its features to the other as a fresh `Core::open` does.
fn pair(clock: &Arc<FakeClock>) -> ((Engine, Db), (Engine, Db)) {
    let ea = engine_seeded(ROOT, [1; 32], Arc::clone(clock));
    let eb = engine_seeded(ROOT, [2; 32], Arc::clone(clock));
    let (mut dba, mut dbb) = (db_root(ROOT), db_root(ROOT));
    trust(&ea, &mut dba, &eb);
    trust(&eb, &mut dbb, &ea);
    assert!(ea.advertise_features(&mut dba).unwrap());
    assert!(eb.advertise_features(&mut dbb).unwrap());
    sync(&dba, &eb, &mut dbb);
    sync(&dbb, &ea, &mut dba);
    ((ea, dba), (eb, dbb))
}

/// Deliver every envelope of `from` that `to` did not write to `to`, in
/// relay order.
fn sync(from: &Db, to: &Engine, to_db: &mut Db) {
    let me = to.keychain.device_id();
    let mut stmt = from
        .conn()
        .prepare("SELECT envelope FROM ops ORDER BY stream_id, device_id, seq")
        .unwrap();
    let envs: Vec<Vec<u8>> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    for env in envs {
        if decode_envelope(&env).unwrap().device_id != me {
            to.apply_remote_all(to_db, &env).unwrap();
        }
    }
}

/// The projection a reader sees: every projection table but the merge state
/// and the version DAG the rows are folded from.
fn rows(db: &Db) -> Vec<(&'static str, Vec<String>)> {
    dump(db.conn())
        .into_iter()
        .filter(|(table, _)| {
            !table.starts_with("merge_")
                && !table.starts_with("entity_")
                && !matches!(*table, "register_losers" | "pending_groups")
        })
        .collect()
}

fn rename(e: &Engine, db: &mut Db, task: EntityRef, title: &str) {
    e.apply(
        db,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                title: Some(title.into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
}

/// A history that writes every projection table: contexts, a stream, tasks
/// with contexts and blockers, a completion, a defer, deletes, a routine and
/// its occurrences, a block bound to a task, an attachment, a focus session
/// with an interruption, a review snapshot and a vault preference.
fn rich_history(e: &Engine, db: &mut Db) {
    legacy_history(e, db);
    e.apply(
        db,
        Command::SetPreference {
            key: "week_start".into(),
            value: PrefValue::Weekday(sunrise_domain::Weekday::Mo),
            target: PrefTarget::Vault,
        },
    )
    .unwrap();
    // A saved view (#341), the other entity written only by `Patch`.
    let view = e
        .apply(
            db,
            Command::CreateSavedView(sunrise_domain::SavedViewDraft {
                name: "Errands".into(),
                view: sunrise_domain::ViewKind::Inbox,
                filter: sunrise_domain::ViewFilter {
                    text: "milk".into(),
                    ..Default::default()
                },
                sort: sunrise_domain::ViewSort::Manual,
            }),
        )
        .unwrap()
        .entity;
    e.apply(
        db,
        Command::UpdateSavedView {
            id: view,
            patch: sunrise_domain::SavedViewPatch {
                name: Some("Errands run".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
}

/// [`rich_history`] without the vault preference, the one entity this build
/// writes as a `Patch`: a history made only of full-state ops.
fn legacy_history(e: &Engine, db: &mut Db) {
    let errands = new_context(e, db, "errands");
    let home = new_context(e, db, "home");
    let work = e
        .apply(
            db,
            Command::CreateStream(StreamDraft {
                name: "Work".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let a = e
        .apply(
            db,
            Command::CreateTask(TaskDraft {
                title: "buy milk".into(),
                contexts: vec![errands, home],
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let b = e
        .apply(
            db,
            Command::CreateTask(TaskDraft {
                title: "write report".into(),
                stream_id: Some(work),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    let c = new_task(e, db, "call the bank");
    set_blockers(e, db, b, vec![a]).unwrap();
    rename(e, db, a, "buy oat milk");
    e.apply(db, Command::CompleteTask(a)).unwrap();
    e.apply(
        db,
        Command::DeferTask {
            id: b,
            to_ms: (NOW + DAY_MS) as u64,
        },
    )
    .unwrap();
    // A manual-order key (#340), so `tasks.sort_order` holds one.
    e.apply(
        db,
        Command::MoveTasks {
            ids: vec![b],
            after: None,
            before: None,
        },
    )
    .unwrap();
    e.apply(db, Command::DeleteTask(c)).unwrap();
    e.apply(db, Command::DeleteContext(home)).unwrap();
    e.apply(
        db,
        Command::CreateRoutine(routine_draft(
            inbox_stream_ref(),
            "FREQ=DAILY",
            NOW,
            RoutineCatchupPolicy::Skip,
            Vec::new(),
        )),
    )
    .unwrap();
    e.apply(
        db,
        Command::MaterializeRoutines {
            now_ms: (NOW + 2 * DAY_MS) as u64,
        },
    )
    .unwrap();
    let block = e
        .apply(
            db,
            Command::CreateBlock(block_draft(9, 2, Some("deep work"))),
        )
        .unwrap()
        .entity;
    e.apply(db, Command::BindTask { block, task: b }).unwrap();
    e.apply(db, Command::AttachFile(attachment_draft(b)))
        .unwrap();
    let session = open_session(e, db, b);
    e.apply(
        db,
        Command::LogInterruption {
            session,
            reason: InterruptionReason::Meeting,
        },
    )
    .unwrap();
    end_session(e, db, session, false);
    e.apply(
        db,
        Command::SaveReviewSnapshot(ReviewSnapshotDraft {
            window_start_ms: (NOW - 7 * DAY_MS) as u64,
            window_end_ms: NOW as u64,
            totals: sunrise_domain::ReviewTotals::default(),
            streams: Vec::new(),
            streaks: Vec::new(),
            note: None,
        }),
    )
    .unwrap();
}

/// Every table the schema has is classified, and every table the registry
/// projects an entity to is classified as a projection (`migrations.md`
/// rule 4).
#[test]
fn every_table_in_the_schema_is_classified() {
    let db = db();
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table'
             AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'search_words_%'
             AND name NOT LIKE 'search_tri_%'
             ORDER BY name",
        )
        .unwrap();
    let schema: BTreeSet<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let classified: BTreeSet<String> = TABLE_CLASSES.iter().map(|(t, _)| (*t).to_owned()).collect();
    assert_eq!(
        schema, classified,
        "a table is unclassified, or a classified table is gone"
    );
    for table in projection_tables() {
        assert!(
            TABLE_CLASSES.contains(&(table, TableClass::Projection)),
            "the registry projects to `{table}`, which a rebuild must clear"
        );
    }
}

/// The acceptance test: a vault built by every kind of command, on two
/// replicas that exchanged their ops, rebuilds to exactly the projection it
/// had, and the identity check holds before and after.
#[test]
fn rebuild_projection_reproduces_every_projection_table() {
    let c = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let ((ea, mut dba), (eb, mut dbb)) = pair(&c);
    rich_history(&ea, &mut dba);
    set_clock(&c, (NOW + 1_000) as u64);
    let from_b = new_task(&eb, &mut dbb, "from b");
    sync(&dba, &eb, &mut dbb);
    sync(&dbb, &ea, &mut dba);
    rename(&ea, &mut dba, from_b, "from b, renamed on a");
    sync(&dba, &eb, &mut dbb);

    for (e, db) in [(&ea, &mut dba), (&eb, &mut dbb)] {
        e.assert_rebuild_identity(db);
        let before = rows(db);
        let empty: Vec<&str> = before
            .iter()
            .filter(|(_, rows)| rows.is_empty())
            .map(|(table, _)| *table)
            .collect();
        assert_eq!(
            empty,
            Vec::<&str>::new(),
            "the history reaches every projection table"
        );
        let ops_before = op_count(db);
        let report = e.rebuild_projection(db).unwrap();
        assert!(report.ops_replayed > 20, "{report:?}");
        assert_eq!(report.ops_skipped, 0);
        assert_eq!(rows(db), before, "the rebuilt projection is the live one");
        assert_eq!(op_count(db), ops_before, "the log is source data");
        e.assert_rebuild_identity(db);
    }
}

/// A rebuild of a rebuilt vault changes nothing, merge state included.
#[test]
fn a_rebuild_is_idempotent() {
    let (e, mut db) = (engine(), db());
    rich_history(&e, &mut db);
    e.rebuild_projection(&mut db).unwrap();
    let once = dump(db.conn());
    e.rebuild_projection(&mut db).unwrap();
    assert_eq!(dump(db.conn()), once);
}

/// A rebuild after compaction folded most of the log keeps what the deleted
/// ops wrote: the base it joins back is the merge state they were folded
/// into.
#[test]
fn rebuild_after_compaction_keeps_the_compacted_state() {
    let c = Arc::new(FakeClock(PLMutex::new(T0)));
    let ((ea, mut dba), (eb, mut dbb)) = pair(&c);
    let errands = new_context(&ea, &mut dba, "errands");
    let t1 = new_task(&ea, &mut dba, "one");
    new_task(&ea, &mut dba, "two");
    new_task(&eb, &mut dbb, "three");
    rename(&ea, &mut dba, t1, "one, renamed");
    ea.apply(
        &mut dba,
        Command::UpdateTask {
            id: t1,
            patch: TaskPatch {
                contexts: Some(vec![errands]),
                ..Default::default()
            },
        },
    )
    .unwrap();
    new_task(&ea, &mut dba, "four");
    sync(&dba, &eb, &mut dbb);
    sync(&dbb, &ea, &mut dba);
    dba.conn()
        .execute("UPDATE outbox SET acked_at_ms = 1", [])
        .unwrap();
    set_clock(&c, T0 + 2 * DAY);
    assert!(eb.publish_stream_digest(&mut dbb, &INBOX).unwrap());
    assert!(eb.publish_stream_digest(&mut dbb, &META_STREAM).unwrap());
    sync(&dbb, &ea, &mut dba);
    let policy = CompactionPolicy {
        retention_ms: DAY,
        ..CompactionPolicy::default()
    };
    let compacted = ea.compact_op_log(&mut dba, &policy).unwrap();
    assert!(compacted.ops_removed >= 4, "{compacted:?}");

    let before = dump(dba.conn());
    ea.assert_rebuild_identity(&dba);
    let report = ea.rebuild_projection(&mut dba).unwrap();
    assert!(report.compacted_entities >= 3, "{report:?}");
    assert_eq!(
        dump(dba.conn()),
        before,
        "compaction folded every row it deleted, so the merge state is whole"
    );
    let t1_row = read_task_t(&ea, &dba, t1);
    assert_eq!(t1_row.title, "one, renamed");
    assert_eq!(
        t1_row.contexts.into_iter().collect::<Vec<_>>(),
        vec![errands]
    );
}

/// Compaction deletes the ops below its floor, and the register losers they
/// wrote stay: the base a rebuild joins is a `doc_state` v2, which carries
/// them with the versions that order them (ADR-0065 §8), so a conflict
/// outlives the ops that caused it.
#[test]
fn a_conflict_outlives_the_ops_that_caused_it() {
    use super::field_merge::{emit_patch, patch, set, text};
    let c = Arc::new(FakeClock(PLMutex::new(T0)));
    let ((ea, mut dba), (eb, mut dbb)) = pair(&c);
    let task = new_task(&ea, &mut dba, "t");
    sync(&dba, &eb, &mut dbb);
    // B's write loses, and B's ops are the ones compaction folds here: A's
    // own `Patch` was never sent, so it stays in the log.
    for (at, e, db, title) in [
        (1_000, &eb, &mut dbb, "from b"),
        (2_000, &ea, &mut dba, "from a"),
    ] {
        set_clock(&c, T0 + at);
        let op = patch(task, false, vec![("title", set(text(title)))]);
        emit_patch(e, db, &INBOX, &op);
    }
    sync(&dba, &eb, &mut dbb);
    sync(&dbb, &ea, &mut dba);
    let losers = |db: &Db| -> Vec<String> {
        dump(db.conn())
            .into_iter()
            .find(|(t, _)| *t == "register_losers")
            .map(|(_, rows)| rows)
            .unwrap_or_default()
    };
    assert_eq!(losers(&dba).len(), 1, "B's title lost to A's, and is kept");

    dba.conn()
        .execute("UPDATE outbox SET acked_at_ms = 1", [])
        .unwrap();
    set_clock(&c, T0 + 2 * DAY);
    assert!(eb.publish_stream_digest(&mut dbb, &INBOX).unwrap());
    sync(&dbb, &ea, &mut dba);
    let policy = CompactionPolicy {
        retention_ms: DAY,
        ..CompactionPolicy::default()
    };
    let compacted = ea.compact_op_log(&mut dba, &policy).unwrap();
    assert!(compacted.ops_removed >= 2, "{compacted:?}");
    let before = losers(&dba);
    ea.assert_rebuild_identity(&dba);
    ea.rebuild_projection(&mut dba).unwrap();
    assert_eq!(
        losers(&dba),
        before,
        "a conflict outlives the ops behind it"
    );
    assert_eq!(read_task_t(&ea, &dba, task).title, "from a");
}

/// A parked op stays parked and out of the projection, and the rows no op
/// derives survive: the Inbox placeholder, a routine's materialization
/// watermark, this device's preference overlay and its blob cache index.
#[test]
fn rebuild_skips_parked_ops_and_keeps_local_only_rows() {
    let c = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let ((ea, mut dba), (eb, mut dbb)) = pair(&c);
    rich_history(&ea, &mut dba);
    super::emit_raw_inner(
        &ea,
        &mut dba,
        &INBOX,
        &super::future_kind_inner("FutureKind"),
    );
    sync(&dba, &eb, &mut dbb);
    eb.apply(
        &mut dbb,
        Command::SetPreference {
            key: "keyboard.vim_mode".into(),
            value: PrefValue::Bool(true),
            target: PrefTarget::Device,
        },
    )
    .unwrap();
    eb.apply(
        &mut dbb,
        Command::MaterializeRoutines {
            now_ms: (NOW + 3 * DAY_MS) as u64,
        },
    )
    .unwrap();
    dbb.conn()
        .execute(
            "INSERT INTO blob_cache (blob_id, sealed_bytes, last_access_ms) VALUES (X'01', 9, 1)",
            [],
        )
        .unwrap();
    let local = |db: &Db| -> Vec<String> {
        let mut out = Vec::new();
        for sql in [
            "SELECT stream_id, created_at_ms FROM streams WHERE lww_device IS NULL",
            "SELECT id, materialized_until_ms FROM routines",
            "SELECT * FROM device_preferences",
            "SELECT * FROM blob_cache",
            "SELECT op_id, reason FROM parked_ops",
        ] {
            let mut stmt = db.conn().prepare(sql).unwrap();
            let n = stmt.column_count();
            let rows = stmt
                .query_map([], |r| {
                    (0..n)
                        .map(|i| {
                            r.get::<_, rusqlite::types::Value>(i)
                                .map(|v| format!("{v:?}"))
                        })
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap();
            for row in rows {
                out.push(format!("{sql}: {}", row.unwrap().join(" | ")));
            }
        }
        out
    };
    let kept = local(&dbb);
    for want in [
        "lww_device IS NULL",
        "routines",
        "device_preferences",
        "blob_cache",
        "parked_ops",
    ] {
        assert!(
            kept.iter().any(|row| row.contains(want)),
            "{want}: {kept:#?}"
        );
    }
    let before = rows(&dbb);
    let report: RebuildReport = eb.rebuild_projection(&mut dbb).unwrap();
    assert_eq!(report.ops_skipped, 0);
    assert_eq!(local(&dbb), kept);
    assert_eq!(rows(&dbb), before);
    eb.assert_rebuild_identity(&dbb);

    // The writer's own copy of the future op is in its log under the kind it
    // was written with, which is not an entity op this build folds.
    ea.assert_rebuild_identity(&dba);
}

/// A damaged op in the log stops the rebuild, and the projection is left as
/// it was.
#[test]
fn an_unreadable_op_rolls_the_rebuild_back() {
    let (e, mut db) = (engine(), db());
    rich_history(&e, &mut db);
    let before = dump(db.conn());
    db.conn()
        .execute(
            "UPDATE ops SET envelope = X'00' WHERE rowid = (
                 SELECT rowid FROM ops WHERE inner_kind IN ('task.update', 'task.patch')
                 ORDER BY rowid DESC LIMIT 1)",
            [],
        )
        .unwrap();
    let err = e.rebuild_projection(&mut db).unwrap_err();
    assert!(matches!(err, EngineError::Invalid(_)), "{err:?}");
    assert_eq!(dump(db.conn()), before);
}

/// What a vault holds the first time a build with the version DAG opens it:
/// the rows and merge state an older build folded, with no versions and no
/// record of a rebuild (migration 0039). With `merge_state`, the merge state
/// is gone too, as in a vault from before migration 0033.
fn as_an_older_build_left_it(db: &Db, merge_state: bool) {
    let mut tables = vec!["entity_versions", "entity_frontier", "projection_rebuild"];
    if merge_state {
        tables.extend([
            "merge_entities",
            "merge_registers",
            "merge_map_entries",
            "merge_orset_adds",
            "merge_orset_removes",
            "merge_counter_deltas",
        ]);
    }
    for table in tables {
        db.conn()
            .execute(&format!("DELETE FROM {table}"), [])
            .unwrap();
    }
}

/// ADR-0065 §7: a vault with only legacy history projects exactly as before
/// after the migration, whose open rebuilds it from the log. Every row reads
/// as it did, whether the vault had merge state (0033) or none, and the
/// merge state and version DAG are the ones the live fold of the same ops
/// holds.
#[test]
fn a_legacy_only_vault_projects_identically_after_the_migration() {
    let c = Arc::new(FakeClock(PLMutex::new(NOW as u64)));
    let ((ea, mut dba), (eb, mut dbb)) = pair(&c);
    legacy_history(&ea, &mut dba);
    set_clock(&c, (NOW + 1_000) as u64);
    let from_b = new_task(&eb, &mut dbb, "from b");
    sync(&dba, &eb, &mut dbb);
    sync(&dbb, &ea, &mut dba);
    rename(&ea, &mut dba, from_b, "from b, renamed on a");
    sync(&dba, &eb, &mut dbb);
    let patches: i64 = dba
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM ops WHERE inner_kind = 'Patch'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(patches, 0, "the history is legacy only");

    for (e, db) in [(&ea, &mut dba), (&eb, &mut dbb)] {
        let live = dump(db.conn());
        let seen = rows(db);
        for merge_state in [false, true] {
            as_an_older_build_left_it(db, merge_state);
            assert_eq!(rows(db), seen, "the rows are the older build's");
            let report = e.rebuild_if_stale(db).unwrap().expect("the open rebuilds");
            assert_eq!(report.ops_skipped, 0);
            assert_eq!(rows(db), seen, "and read exactly as they did");
            assert_eq!(dump(db.conn()), live);
            assert_eq!(e.rebuild_if_stale(db).unwrap(), None);
        }
    }
}

/// A build before ADR-0065 PR 3 wrote a command's row and logged its op
/// without folding it, so its vault can hold a row newer than the merge
/// state behind it. The rebuild at open folds the op from the log, where
/// the first op to touch the entity used to fold the row.
#[test]
fn a_row_an_older_build_never_folded_is_folded_by_the_rebuild() {
    let (e, mut db) = (engine(), db());
    let task = new_task(&e, &mut db, "first");
    let state: Vec<&str> = TABLE_CLASSES
        .iter()
        .map(|(table, _)| *table)
        .filter(|table| table.starts_with("merge_") || table.starts_with("entity_"))
        .collect();
    for table in &state {
        db.conn()
            .execute_batch(&format!(
                "CREATE TEMP TABLE stale_{table} AS SELECT * FROM {table}"
            ))
            .unwrap();
    }
    rename(&e, &mut db, task, "second");
    let live = dump(db.conn());
    let seen = rows(&db);

    // Put the merge state back as it was before the rename: the row says
    // "second", and nothing behind it does.
    as_an_older_build_left_it(&db, true);
    for table in &state {
        db.conn()
            .execute_batch(&format!("INSERT INTO {table} SELECT * FROM stale_{table}"))
            .unwrap();
    }
    assert_eq!(rows(&db), seen);
    assert_ne!(dump(db.conn()), live, "the merge state is behind the row");

    e.rebuild_if_stale(&mut db)
        .unwrap()
        .expect("the open rebuilds");
    assert_eq!(dump(db.conn()), live);
    assert_eq!(read_task_t(&e, &db, task).title, "second");
}

/// Rebuild equivalence over committed vaults the engine wrote
/// (`migrations.md` §Migration rigor rule 1).
///
/// `#[cfg(test)]` restates what this module already is, for
/// `core-filesystem-gate.py`, which skips a test module by that attribute:
/// the fixture is copied out of the tree, and that is a test reading a file,
/// not the core reaching past its storage handle.
#[cfg(test)]
mod fixture;

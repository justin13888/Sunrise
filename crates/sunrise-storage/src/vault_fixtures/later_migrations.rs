//! The tests of later migrations, each over the committed fixture at the
//! version before it.

use super::contents::{STREAM_ALPHA, TASK_IN_ALPHA};
use super::seed::{BLOCK_NO_STREAM, INBOX_SENTINEL, TASK_NO_STREAM};
use super::{fixture_name, open_migrated, row_count};

/// Migration 0041 (#506, ADR-0066 §2) over a real v40 vault: the routine's
/// times gain the kinds their bare instants mean, a zoned anchor and end in
/// the routine's own zone and an instant pause and last completion, the
/// stream's pause is an instant, and every `*_civil` column starts NULL, for
/// the rebuild at the next open to fill from the ops.
#[test]
fn a_v40_vaults_routine_and_stream_times_gain_their_kinds_and_empty_civil_columns() {
    let (_dir, db) = open_migrated(&fixture_name(40));
    let one = |sql: &str| -> Option<String> {
        db.conn()
            .query_row(sql, [], |r| r.get(0))
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    };
    let zone = one("SELECT timezone FROM routines");
    assert_eq!(
        one("SELECT starts_at_kind FROM routines").as_deref(),
        Some("zoned")
    );
    assert_eq!(
        one("SELECT starts_at_tz FROM routines"),
        zone,
        "pinned in its zone"
    );
    assert_eq!(
        one("SELECT ends_at_kind FROM routines").as_deref(),
        Some("zoned")
    );
    assert_eq!(one("SELECT ends_at_tz FROM routines"), zone);
    assert_eq!(
        one("SELECT paused_until_kind FROM routines").as_deref(),
        Some("instant")
    );
    assert_eq!(
        one("SELECT last_completed_at_kind FROM routines").as_deref(),
        Some("instant")
    );
    assert_eq!(
        one("SELECT paused_until_kind FROM streams WHERE paused_until_ms IS NOT NULL").as_deref(),
        Some("instant")
    );
    for (table, col) in [
        ("tasks", "planned_at_civil"),
        ("tasks", "hard_due_at_civil"),
        ("tasks", "completed_at_civil"),
        ("blocks", "starts_at_civil"),
        ("blocks", "ends_at_civil"),
        ("routines", "starts_at_civil"),
        ("routines", "ends_at_civil"),
        ("streams", "paused_until_civil"),
    ] {
        let set: i64 = db
            .conn()
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE {col} IS NOT NULL"),
                [],
                |r| r.get(0),
            )
            .unwrap_or_else(|e| panic!("`{table}.{col}` after 0041: {e}"));
        assert_eq!(set, 0, "`{table}.{col}` starts NULL");
    }
}

/// Migration 0042 (#340) over a real v41 vault: every task gains a
/// `sort_order` that starts NULL, never placed, with no key invented for it,
/// and the manual-order index exists.
#[test]
fn a_v41_vaults_tasks_gain_an_unplaced_sort_order_and_its_index() {
    let (_dir, db) = open_migrated(&fixture_name(41));
    let placed: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM tasks WHERE sort_order IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(placed, 0, "no task is placed by the migration");
    assert!(row_count(&db, "tasks") > 0, "the fixture's tasks survive");
    let index: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM sqlite_master
             WHERE type = 'index' AND name = 'tasks_by_stream_sort_order'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(index, 1);
}

/// Migration 0043 (#341) over a real v42 vault: the `saved_views` table
/// exists, empty, with the index the list reads.
#[test]
fn a_v42_vault_gains_an_empty_saved_views_table() {
    let (_dir, db) = open_migrated(&fixture_name(42));
    assert_eq!(row_count(&db, "saved_views"), 0);
    assert!(row_count(&db, "tasks") > 0, "the fixture's tasks survive");
    let index: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM sqlite_master
             WHERE type = 'index' AND name = 'saved_views_live'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(index, 1);
}

/// Migration 0044 (#332, ADR-0046) over a real v43 vault: a task and a block
/// in the Inbox sentinel belong to no stream after it, their bindings survive
/// the table rebuild, the sentinel's synthetic `streams` row is gone, every
/// other row keeps its stream, and the two new columns start NULL. Every
/// index the rebuilt tables had is back.
#[test]
fn a_v43_vaults_inbox_rows_have_no_stream_after_0044() {
    let (_dir, db) = open_migrated(&fixture_name(43));
    let stream_of = |table: &str, id: &[u8; 16]| -> Option<Vec<u8>> {
        db.conn()
            .query_row(
                &format!("SELECT stream_id FROM {table} WHERE id = ?"),
                rusqlite::params![id.to_vec()],
                |r| r.get(0),
            )
            .unwrap_or_else(|e| panic!("`{table}` row: {e}"))
    };
    assert_eq!(stream_of("tasks", &TASK_NO_STREAM), None);
    assert_eq!(stream_of("blocks", &BLOCK_NO_STREAM), None);
    assert_eq!(
        stream_of("tasks", &TASK_IN_ALPHA),
        Some(STREAM_ALPHA.to_vec()),
        "a task in a real stream keeps it"
    );
    let sentinel_rows: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM streams WHERE stream_id = ?",
            rusqlite::params![INBOX_SENTINEL.to_vec()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sentinel_rows, 0, "the private domain has no stream row");
    let alpha_rows: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM streams WHERE stream_id = ? AND rehomed_to IS NULL",
            rusqlite::params![STREAM_ALPHA.to_vec()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(alpha_rows, 1, "a stream keeps its row, re-homing nothing");
    let memberships: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM task_contexts WHERE task_id IN (?, ?)",
            rusqlite::params![TASK_NO_STREAM.to_vec(), TASK_IN_ALPHA.to_vec()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        memberships, 2,
        "the rebuild carries every membership across"
    );
    let bindings: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM block_tasks WHERE block_id = ?",
            rusqlite::params![BLOCK_NO_STREAM.to_vec()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bindings, 1, "and every binding");
    let triaged: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM tasks WHERE triaged_at_ms IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(triaged, 0, "no task is triaged by the migration");
    for index in [
        "tasks_by_stream",
        "tasks_by_due",
        "tasks_by_stream_sort_order",
        "blocks_by_time",
        "blocks_by_end",
    ] {
        let n: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = ?",
                [index],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "`{index}` is rebuilt");
    }
    let leftovers: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name LIKE '%_0044'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leftovers, 0, "no scratch table outlives the migration");
}

/// Migration 0045 (#334, ADR-0047 §1) over a real v44 vault: the plan and
/// the hard deadline are the old `scheduled_at` and `due_at`, every one of
/// their four columns carried across byte for byte; the soft deadline starts
/// NULL for every task, so no task gains a target; and the two deadline
/// indexes exist, `tasks_by_due` now over the hard deadline.
#[test]
fn a_v44_vaults_task_times_are_the_plan_and_the_hard_deadline_after_0045() {
    let (_dir, db) = open_migrated(&fixture_name(44));
    let row: (i64, String, String, String, i64, String, String, String) = db
        .conn()
        .query_row(
            "SELECT planned_at_ms, planned_at_kind, planned_at_tz, planned_at_civil,
                    hard_due_at_ms, hard_due_at_kind, hard_due_at_tz, hard_due_at_civil
             FROM tasks WHERE id = ?",
            rusqlite::params![TASK_IN_ALPHA.to_vec()],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )
        .expect("the seeded task");
    assert_eq!(
        row,
        (
            1_700_000_600_000,
            "instant".into(),
            "Europe/Paris".into(),
            "2023-11-15T01:00:00".into(),
            1_700_000_700_000,
            "date".into(),
            "UTC".into(),
            "2023-11-16".into(),
        ),
        "scheduled_at is planned_at and due_at is hard_due_at, unchanged"
    );
    let targets: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM tasks
             WHERE target_at_ms IS NOT NULL OR target_at_kind IS NOT NULL
                OR target_at_tz IS NOT NULL OR target_at_civil IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(targets, 0, "no task gets a target_at");
    let old: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM pragma_table_info('tasks')
             WHERE name LIKE 'scheduled_at%' OR name LIKE 'due_at%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(old, 0, "no column keeps an old name");
    for (index, column) in [
        ("tasks_by_due", "hard_due_at_ms"),
        ("tasks_by_target", "target_at_ms"),
    ] {
        let sql: String = db
            .conn()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?",
                [index],
                |r| r.get(0),
            )
            .unwrap_or_else(|e| panic!("`{index}`: {e}"));
        assert!(sql.contains(column), "`{index}` indexes `{column}`: {sql}");
    }
}

/// Migration 0046 (#335, ADR-0047 §4) over a real v45 vault: every task
/// gains a drop reason and a triage acknowledgement that start NULL, so no
/// task reads as dropped with a reason or kept, and every other column is as
/// it was.
#[test]
fn a_v45_vaults_tasks_gain_an_empty_drop_reason_and_keep_after_0046() {
    let (_dir, db) = open_migrated(&fixture_name(45));
    let set: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM tasks
             WHERE cancel_reason IS NOT NULL OR late_acknowledged_at_ms IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        set, 0,
        "no task is dropped with a reason or kept by the migration"
    );
    let target: Option<String> = db
        .conn()
        .query_row(
            "SELECT target_at_kind FROM tasks WHERE id = ?",
            rusqlite::params![TASK_IN_ALPHA.to_vec()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        target.as_deref(),
        Some("all_day"),
        "0045's columns are kept"
    );
}

/// Migration 0047 (#331) over a real v46 vault: every existing task gains a
/// hard-constraint flag of 0, none having been generated with it, and every
/// existing routine a `split_from` of NULL, none having been split. The
/// fixture holds tasks and routines, so neither assertion is vacuous.
#[test]
fn a_v46_vaults_tasks_gain_an_unviolated_hard_constraint_and_routines_no_split_after_0047() {
    let (_dir, db) = open_migrated(&fixture_name(46));
    let count = |sql: &str| -> i64 {
        db.conn()
            .query_row(sql, [], |r| r.get(0))
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    };
    let tasks = row_count(&db, "tasks");
    assert!(tasks > 0, "the fixture's tasks survive");
    assert_eq!(
        count("SELECT count(*) FROM tasks WHERE hard_constraint_violated = 0"),
        tasks,
        "every task starts unviolated"
    );
    let routines = row_count(&db, "routines");
    assert!(routines > 0, "the fixture's routines survive");
    assert_eq!(
        count("SELECT count(*) FROM routines WHERE split_from IS NULL"),
        routines,
        "no routine starts split"
    );
}

//! The migration history's invariants, and each step run against the schema
//! the step before it left.

use std::path::Path;

use super::*;
use crate::store::Store;

/// The ids are the versions, so a gap or a reordering would stamp a
/// database with a version whose step never ran.
#[test]
fn migration_ids_count_up_from_one_without_gaps() {
    for (i, m) in MIGRATIONS.iter().enumerate() {
        assert_eq!(
            m.id,
            u32::try_from(i + 1).unwrap(),
            "migration {} is out of place",
            m.name
        );
    }
    assert_eq!(LATEST, u32::try_from(MIGRATIONS.len()).unwrap());
}

/// 0004 counts the ops of the frames stored before it, so a replay of
/// them is counted as a replay of a frame stored after it would be; a
/// frame that is not an op batch counts none.
#[test]
fn relay_frames_n_ops_counts_the_frames_already_stored() {
    use sunrise_wire_protocol::{encode_frame, FrameFlags, MsgKind, OpBatchPayload};
    let batch = OpBatchPayload {
        ops: vec![vec![1; 8], vec![2; 8], vec![3; 8]],
        batch_id: 1,
        stream_id: [0x11; 16],
    };
    let frame = encode_frame(
        MsgKind::OpBatch,
        FrameFlags::EMPTY,
        &batch.encode().unwrap(),
    )
    .unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    migrate_to(&mut conn, 3).unwrap();
    for bytes in [frame.as_slice(), b"not a frame".as_slice()] {
        conn.execute(
            "INSERT INTO relay_frames (account_h, stream_id, bytes, n_bytes, created_ms)
                 VALUES (x'00', x'11', ?1, 0, 0)",
            [bytes],
        )
        .unwrap();
    }
    migrate(&mut conn).unwrap();
    let counts: Vec<i64> = conn
        .prepare("SELECT n_ops FROM relay_frames ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(counts, [3, 0]);
}

/// Running the runner twice is the restart case, and must change nothing.
#[test]
fn a_migrated_database_is_left_alone_by_a_second_run() {
    let mut conn = Connection::open_in_memory().unwrap();
    assert_eq!(migrate(&mut conn).unwrap(), 0);
    let before = schema_of(&conn);
    assert_eq!(migrate(&mut conn).unwrap(), i64::from(LATEST));
    assert_eq!(schema_of(&conn), before);
    assert_eq!(version(&conn).unwrap(), i64::from(LATEST));
}

/// A step that fails leaves the database at the version before it, with
/// nothing of the step applied: the stamp and the DDL share one commit.
#[test]
fn a_failing_step_leaves_the_version_where_it_was() {
    let mut conn = Connection::open_in_memory().unwrap();
    // A `devices` table from before the column, which 0001 leaves alone
    // and 0002 has to extend.
    conn.execute_batch(
        "CREATE TABLE devices (device_id TEXT PRIMARY KEY, account_id TEXT NOT NULL);",
    )
    .unwrap();
    migrate_to(&mut conn, 1).unwrap();
    // The index name 0002 creates last, taken by a table, so the step
    // fails *after* its `ALTER TABLE` has run.
    conn.execute_batch("CREATE TABLE devices_by_vault_id (x);")
        .unwrap();

    assert!(migrate(&mut conn).is_err());

    assert_eq!(version(&conn).unwrap(), 1);
    let column: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('devices') WHERE name = 'vault_device_id'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(column, 0, "the step's ALTER TABLE must have rolled back");
}

/// The DDL every relay before the migration runner executed on each start,
/// copied as it stood and frozen here, so the fixtures below are what an
/// operator's file actually holds rather than whatever the constants say
/// today. `{VAULT_COLUMN}` is where the one later column went.
const PRE_RUNNER_DDL: &str = "
        CREATE TABLE IF NOT EXISTS accounts (
            account_id     TEXT PRIMARY KEY,
            oidc_iss       TEXT NOT NULL,
            oidc_sub       TEXT NOT NULL,
            email          TEXT,
            identity_pub_s TEXT,
            identity_pub_d TEXT,
            recovery_blob  TEXT,
            tier           TEXT NOT NULL DEFAULT 'free',
            terms_at_ms    INTEGER,
            created_at_ms  INTEGER NOT NULL,
            UNIQUE (oidc_iss, oidc_sub)
        );
        CREATE TABLE IF NOT EXISTS devices (
            device_id       TEXT PRIMARY KEY,
            account_id      TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
            device_pub_s    TEXT NOT NULL,
            device_pub_d    TEXT,
            device_cert     TEXT,
            {VAULT_COLUMN}
            nickname        TEXT NOT NULL,
            platform        TEXT NOT NULL,
            app_version     TEXT,
            created_at_ms   INTEGER NOT NULL,
            last_seen_at_ms INTEGER NOT NULL,
            revoked         INTEGER NOT NULL DEFAULT 0,
            revoked_at_ms   INTEGER
        );
        CREATE INDEX IF NOT EXISTS devices_by_account ON devices(account_id);
        CREATE TABLE IF NOT EXISTS push_tokens (
            device_id     TEXT NOT NULL REFERENCES devices(device_id) ON DELETE CASCADE,
            platform      TEXT NOT NULL,
            token         TEXT NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            PRIMARY KEY (device_id, platform)
        );
        CREATE TABLE IF NOT EXISTS relay_frames (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            account_h  BLOB NOT NULL,
            stream_id  BLOB NOT NULL,
            bytes      BLOB NOT NULL,
            n_bytes    INTEGER NOT NULL,
            created_ms INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS relay_frames_by_channel
            ON relay_frames(account_h, stream_id, id);
        CREATE TABLE IF NOT EXISTS relay_frame_heads (
            frame_id  INTEGER NOT NULL REFERENCES relay_frames(id) ON DELETE CASCADE,
            device_id BLOB NOT NULL,
            max_seq   INTEGER NOT NULL,
            PRIMARY KEY (frame_id, device_id)
        );
        CREATE TABLE IF NOT EXISTS relay_evicted (
            account_h       BLOB NOT NULL,
            stream_id       BLOB NOT NULL,
            device_id       BLOB NOT NULL,
            evicted_through INTEGER NOT NULL,
            PRIMARY KEY (account_h, stream_id, device_id)
        );
        CREATE TABLE IF NOT EXISTS relay_batches (
            account_h     BLOB NOT NULL,
            stream_id     BLOB NOT NULL,
            ops_h         BLOB NOT NULL,
            frame_id      INTEGER NOT NULL REFERENCES relay_frames(id) ON DELETE CASCADE,
            batch_id      INTEGER NOT NULL,
            first_seen_ms INTEGER NOT NULL,
            PRIMARY KEY (account_h, stream_id, ops_h)
        );
        CREATE INDEX IF NOT EXISTS relay_batches_by_frame ON relay_batches(frame_id);";

/// One row in every table, so a migration that drops or rewrites one is
/// caught by a count.
const FIXTURE_ROWS: &str = "
        INSERT INTO accounts (account_id, oidc_iss, oidc_sub, email, created_at_ms)
            VALUES ('A1', 'https://idp.example', 'alice', 'a@example.com', 1);
        INSERT INTO devices (device_id, account_id, device_pub_s, nickname, platform,
                             created_at_ms, last_seen_at_ms)
            VALUES ('D1', 'A1', 'k', 'laptop', 'linux', 1, 2);
        INSERT INTO push_tokens VALUES ('D1', 'fcm', 'tok', 3);
        INSERT INTO relay_frames (account_h, stream_id, bytes, n_bytes, created_ms)
            VALUES (x'01', x'02', x'0304', 2, 4);
        INSERT INTO relay_frame_heads VALUES (1, x'05', 7);
        INSERT INTO relay_evicted VALUES (x'01', x'02', x'05', 6);
        INSERT INTO relay_batches VALUES (x'01', x'02', x'06', 1, 1, 4);";

const TABLES: [&str; 7] = [
    "accounts",
    "devices",
    "push_tokens",
    "relay_frames",
    "relay_frame_heads",
    "relay_evicted",
    "relay_batches",
];

/// A database as a pre-runner release left it: `user_version = 0`, the
/// frozen DDL, one row per table.
fn pre_runner_fixture(path: &Path, with_vault_column: bool) {
    let conn = Connection::open(path).unwrap();
    let column = if with_vault_column {
        "vault_device_id TEXT,"
    } else {
        ""
    };
    conn.execute_batch(&PRE_RUNNER_DDL.replace("{VAULT_COLUMN}", column))
        .unwrap();
    if with_vault_column {
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS devices_by_vault_id
                     ON devices(account_id, vault_device_id);",
        )
        .unwrap();
    }
    conn.execute_batch(FIXTURE_ROWS).unwrap();
}

fn version_of(s: &Store) -> i64 {
    version(&s.conn.lock()).unwrap()
}

fn assert_every_row_kept(s: &Store) {
    let conn = s.conn.lock();
    for table in TABLES {
        let n: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "{table} lost or gained rows in the upgrade");
    }
    let (email, token): (String, String) = conn
        .query_row(
            "SELECT a.email, p.token FROM accounts a
                 JOIN devices d ON d.account_id = a.account_id
                 JOIN push_tokens p ON p.device_id = d.device_id",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((email.as_str(), token.as_str()), ("a@example.com", "tok"));
}

/// **Both shapes of database a released relay can have left behind** open
/// under the runner, reach the newest version, and keep every row: the one
/// from the last release, whose `devices` has `vault_device_id`, and the one
/// from before that column existed.
#[test]
fn pre_runner_databases_adopt_the_history_and_keep_every_row() {
    for with_vault_column in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sunrise.db");
        pre_runner_fixture(&path, with_vault_column);

        let s = Store::open(Some(&path)).unwrap();

        assert_eq!(version_of(&s), i64::from(LATEST));
        assert_every_row_kept(&s);
        let d = s.active_device("A1", "D1").unwrap().unwrap();
        assert_eq!(d.vault_device_id, None);
    }
}

/// **Every migration runs forward from every earlier version**, and every
/// path ends at the schema a fresh database gets.
///
/// The second half is what freezes migration 0001: its DDL is the tenants'
/// `SCHEMA` constants, and an edit to one changes what a fresh database
/// holds without changing what an upgraded one does. That divergence is a
/// relay whose behaviour depends on the date it was installed, and it fails
/// here — the pre-runner fixtures are built from the frozen copy above, not
/// from the constants.
#[test]
fn fresh_and_upgraded_databases_reach_the_same_schema() {
    let fresh = Store::open(None).unwrap();
    let fresh_schema = schema_of(&fresh.conn.lock());

    for with_vault_column in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sunrise.db");
        pre_runner_fixture(&path, with_vault_column);
        let s = Store::open(Some(&path)).unwrap();
        assert_eq!(
            schema_of(&s.conn.lock()),
            fresh_schema,
            "a pre-runner database (vault column: {with_vault_column}) upgraded \
                 to a different schema than a fresh one"
        );
    }

    for start in 0..LATEST {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sunrise.db");
        {
            let mut conn = Connection::open(&path).unwrap();
            migrate_to(&mut conn, start).unwrap();
            if start >= 1 {
                conn.execute_batch(FIXTURE_ROWS).unwrap();
            }
        }
        let s = Store::open(Some(&path)).unwrap();
        assert_eq!(version_of(&s), i64::from(LATEST));
        assert_eq!(
            schema_of(&s.conn.lock()),
            fresh_schema,
            "migrating forward from version {start}"
        );
        if start >= 1 {
            assert_every_row_kept(&s);
        }
    }
}

/// **The comparison sees every constraint an edit to a frozen `SCHEMA` can
/// add**, so the convergence test above stands in for a checksum: a
/// `CHECK`, a `UNIQUE` constraint, `AUTOINCREMENT`, a unique index and a
/// partial index each change [`schema_of`], and a change of layout alone
/// does not.
#[test]
fn the_schema_comparison_sees_constraints_and_ignores_layout() {
    let of = |ddl: &str| {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(ddl).unwrap();
        schema_of(&conn)
    };
    let base = of("CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT);
             CREATE INDEX t_a ON t(a);");

    assert_eq!(
        of("CREATE TABLE t (
                    id INTEGER PRIMARY KEY, -- the key; CHECK (no) AUTOINCREMENT
                    a  TEXT,
                    b  TEXT
                );
                CREATE INDEX t_a ON t ( a );"),
        base,
        "layout and comments are not schema"
    );
    for (what, ddl) in [
        (
            "a CHECK",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT CHECK (a <> ''), b TEXT);
                 CREATE INDEX t_a ON t(a);",
        ),
        (
            "a UNIQUE constraint",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT, UNIQUE (a, b));
                 CREATE INDEX t_a ON t(a);",
        ),
        (
            "AUTOINCREMENT",
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, a TEXT, b TEXT);
                 CREATE INDEX t_a ON t(a);",
        ),
        (
            "a unique index",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT);
                 CREATE UNIQUE INDEX t_a ON t(a);",
        ),
        (
            "a partial index",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT);
                 CREATE INDEX t_a ON t(a) WHERE a IS NOT NULL;",
        ),
    ] {
        assert_ne!(of(ddl), base, "{what} went unnoticed");
    }
    assert_ne!(
        of("CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT CHECK (a <> 'x'), b TEXT);"),
        of("CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT CHECK (a <> 'y'), b TEXT);"),
        "a CHECK's expression is compared, not only its presence"
    );
}

/// **A database from a newer release is refused, and not one byte of it
/// changes.** Not even the journal mode: switching to WAL rewrites the
/// header, which is why the version is read first.
#[test]
fn a_database_from_a_newer_release_is_refused_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sunrise.db");
    let future = i64::from(LATEST) + 1;
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE from_the_future (x); PRAGMA user_version = {future};"
        ))
        .unwrap();
    }
    let before = std::fs::read(&path).unwrap();

    let refused = Store::open(Some(&path));

    assert!(
        matches!(
            refused,
            Err(StoreError::SchemaTooNew { found, supported })
                if found == future && supported == LATEST
        ),
        "{refused:?}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a refused database must be left byte-for-byte as it was"
    );
    assert!(
        !dir.path().join("sunrise.db-wal").exists(),
        "and no WAL beside it either"
    );
}

/// A negative version is no version this relay ever wrote.
#[test]
fn a_negative_schema_version_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sunrise.db");
    Connection::open(&path)
        .unwrap()
        .execute_batch("PRAGMA user_version = -1;")
        .unwrap();
    assert!(matches!(
        Store::open(Some(&path)),
        Err(StoreError::SchemaTooNew { found: -1, .. })
    ));
}

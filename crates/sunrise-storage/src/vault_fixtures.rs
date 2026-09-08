//! Real old vaults, checked in, and the chain run over them.
//!
//! Everything else in this crate tests migrations against a schema this build
//! just built. `db.rs` replays `MIGRATIONS[n].sql` by hand into a fresh
//! in-memory connection, and `tests/vault_at_rest.rs` opens a *current* vault
//! whose version stamp has been lied down. Neither is a 13-era vault: the
//! first never goes through `Db::open`, and the second has every column the
//! migrations were supposed to add already present. So until this module the
//! seven migrations from 13 to 20 had never been run against the thing they
//! exist for — old data, written by an old schedule, opened by this build.
//!
//! A fixture is the only honest way to close that. Each one is an encrypted
//! `SQLCipher` vault, produced by `Db::create_at_storage_v` at some historical
//! `STORAGE_V`, seeded with rows this file also states as expectations, and
//! committed. The tests copy it to a temporary directory, call the ordinary
//! public `Db::open` on the copy, and assert on the *contents* afterwards —
//! not on the fact that the open returned `Ok`. A migration that drops a
//! column's data opens perfectly.
//!
//! # The key is committed, on purpose
//!
//! `FIXTURE_VAULT_ROOT` is the vault root key for every file under
//! `crates/sunrise-storage/fixtures/`, and it is in this repository in plain
//! sight because that is what a test vault is for. There is nothing behind it:
//! the rows are the invented ones below, and no user, device or account has
//! ever been keyed with it. It is **not** a leaked secret and it must not be
//! "fixed" — rotating it would only mean regenerating the fixtures under a
//! different constant that is equally public, and the same note is repeated in
//! `fixtures/README.md` so that a reader who finds the binary before the code
//! learns it there too.
//!
//! # Regenerating
//!
//! `mise run storage-fixtures`, which runs `regenerate_the_committed_vault_fixtures`
//! — an `#[ignore]`d test, so an ordinary `cargo test` reads the committed
//! files and never rewrites them. Regenerate deliberately, and only for a
//! reason: a fixture rewritten at today's schema stops being old, and the
//! chain it is supposed to exercise then runs over nothing. There is no reason
//! to regenerate the v13 file at all short of the baseline itself changing,
//! which ADR-0018 says happens once and has happened.
//!
//! # Which versions get a fixture
//!
//! Two: the oldest supported version, and every later version whose successor
//! migrations move *data* that the older fixture cannot contain. Of 0014-0020,
//! three move data rather than only schema — 0014 backfills
//! `streams.sort_order`, 0017 drops `stream_keys`, carries `revoked_at_ms`
//! into `device_revocations` and re-points the legacy Inbox, and 0019
//! backfills `identity.minted_by_device_id` and blanks `id_d_priv_wrapped` —
//! while 0015, 0016, 0018 and 0020 are pure `ALTER TABLE ADD COLUMN` /
//! `CREATE TABLE`.
//!
//! The v13 fixture exercises the first two. It cannot exercise the third: the
//! `identity` table does not exist before 0017 and 0017 creates it empty, so
//! 0019's `UPDATE` matches no row on any vault that started at 13. That is the
//! whole argument for the second fixture, and 17 rather than 18 because 0018
//! is schema-only and starting a version earlier costs nothing.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use sunrise_cbor::version::STORAGE_V;
use sunrise_crypto::keys::VaultRootKey;

use crate::db::Db;

/// The vault root every committed fixture is keyed with.
///
/// Public in the repository on purpose; see this module's header.
const FIXTURE_VAULT_ROOT: [u8; 32] = [0x7e; 32];

/// A vault written when `STORAGE_V` was 13, the ADR-0018 baseline.
const V13_FIXTURE: &str = "vault_storage_v13.db";

/// A vault written when `STORAGE_V` was 17, the key hierarchy (ADR-0024).
const V17_FIXTURE: &str = "vault_storage_v17.db";

/// A `streams` row's `(name, extra, description, default_context)`: the name
/// 0013 wrote and the three columns 0015 and 0016 add beside it.
type StreamAdditiveColumns = (String, Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>);

/// The whole `identity` row, in the order 0019 decides its fate:
/// `(identity_id, id_s_pub, id_d_pub, id_s_priv_wrapped, id_d_priv_wrapped,
/// minted_by_device_id)`.
type IdentityRow = (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);

/// A `stream_keys` row's `(stream_id, key_id, wrapped, source)`.
type StreamKeyRow = (Vec<u8>, Vec<u8>, Vec<u8>, String);

/// A `local_identity` row's
/// `(device_id, signing_secret_wrapped, cert_blob, dh_secret_wrapped)`.
type LocalIdentityRow = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);

// --- the known contents ---------------------------------------------------
//
// Every value below is written by the generator and asserted by the tests, so
// "unchanged" means "still equal to the constant the fixture was built from"
// rather than "equal to whatever we read a moment ago".

/// The pre-0017 Inbox: sixteen zero bytes, which was also the vault-meta
/// stream id.
const LEGACY_INBOX: [u8; 16] = [0u8; 16];
/// Where 0017 moves it.
const REMAPPED_INBOX: [u8; 16] = *b"\x00\x00\x00sunrise.inbox";

const STREAM_ALPHA: [u8; 16] = [0xa1; 16];
const STREAM_BETA: [u8; 16] = [0xb2; 16];
const STREAM_GAMMA: [u8; 16] = [0xc3; 16];

const TASK_IN_INBOX: [u8; 16] = [0x01; 16];
const TASK_IN_ALPHA: [u8; 16] = [0x02; 16];
const INBOX_TASK_TITLE: &str = "Renew the passport";
const ALPHA_TASK_TITLE: &str = "Ship the migration fixture";
/// A `NoteBody`, opaque to SQL and to this test — which is the point: it is
/// checked byte for byte.
const ALPHA_TASK_BODY: &[u8] = b"\x82\x01\x6fblock-grammar-v1";

const CONTEXT_ERRANDS: [u8; 16] = [0xd4; 16];

const DEVICE_LAPTOP: [u8; 16] = [0xf6; 16];
const DEVICE_RETIRED: [u8; 16] = [0xf7; 16];
const RETIRED_AT_MS: i64 = 1_700_000_000_000;

const OP_ID: [u8; 16] = [0xe5; 16];
/// A sealed envelope. Nothing in the storage layer may rewrite these bytes —
/// the signature covers them — so this is the strictest content assertion in
/// the file.
const OP_ENVELOPE: &[u8] = b"\xa4\x01\x58\x20sealed-op-envelope-not-rewritable";

const SIGNING_SECRET_WRAPPED: &[u8] = b"\x11\x22\x33wrapped-device-signing-seed";
const DEVICE_CERT_BLOB: &[u8] = b"\xa3\x01\x50self-issued-device-cert";

// --- v17-only contents ----------------------------------------------------

/// The vault-meta stream: sixteen zero bytes, and never a `streams` row.
const VAULT_META_STREAM: [u8; 16] = [0u8; 16];
const IDENTITY_ID: [u8; 16] = [0x21; 16];
const ID_S_PUB: [u8; 32] = [0x22; 32];
const ID_D_PUB: [u8; 32] = [0x23; 32];
const ID_S_PRIV_WRAPPED: &[u8] = b"\x24wrapped-ID_S_priv";
/// The one the 0019 migration decides the fate of.
const ID_D_PRIV_WRAPPED: &[u8] = b"\x25wrapped-ID_D_priv";
const DH_SECRET_WRAPPED: &[u8] = b"\x26wrapped-device-X25519-secret";
const META_KEY_ID: &[u8] = b"\x01\x02\x03\x04\x05\x06\x07\x08";
const ALPHA_KEY_ID: &[u8] = b"\x08\x07\x06\x05\x04\x03\x02\x01";
const WRAPPED_STREAM_KEY: &[u8] = b"\x27wrapped-stream-key";

/// The directory the committed fixtures live in.
fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn fixture_key() -> VaultRootKey {
    VaultRootKey::from_bytes(FIXTURE_VAULT_ROOT)
}

/// Copy `name` out of the committed fixtures and open the copy through the
/// ordinary public entry point, applying whatever migrations it needs.
///
/// The copy is not politeness: `Db::open` migrates in place, so a test that
/// opened the committed file would upgrade it and every later run would be
/// reading a current vault while believing it was reading an old one. The
/// `TempDir` is returned because dropping it deletes the copy.
fn open_migrated(name: &str) -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    let source = fixture_dir().join(name);
    fs::copy(&source, &path).unwrap_or_else(|e| {
        panic!(
            "copy {}: {e} — run `mise run storage-fixtures`",
            source.display()
        )
    });
    let db = Db::open(&path, &fixture_key())
        .unwrap_or_else(|e| panic!("the migration chain must open `{name}`: {e}"));
    (dir, db)
}

fn stamped_storage_v(db: &Db) -> u32 {
    db.conn()
        .query_row("SELECT storage_v FROM schema_meta", [], |r| r.get(0))
        .expect("read schema_meta")
}

fn table_exists(db: &Db, table: &str) -> bool {
    let n: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
            rusqlite::params![table],
            |r| r.get(0),
        )
        .expect("probe sqlite_master");
    n == 1
}

fn row_count(db: &Db, table: &str) -> i64 {
    db.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("count {table}: {e}"))
}

// --- seeding --------------------------------------------------------------

/// The rows a 13-era vault holds.
///
/// Chosen so that every data-moving thing 0014 and 0017 do has something to
/// move: streams out of alphabetical order and one of them the lazily created
/// Inbox placeholder, a task parked in the legacy Inbox stream, a revoked
/// device, and a pre-hierarchy `stream_keys` row.
fn seed_v13(conn: &Connection) {
    for (id, name) in [
        (LEGACY_INBOX, ""),
        (STREAM_GAMMA, "Gamma"),
        (STREAM_BETA, "beta"),
        (STREAM_ALPHA, "Alpha"),
    ] {
        conn.execute(
            "INSERT INTO streams
             (stream_id, head_root, last_op_seq, name, created_at_ms, updated_at_ms)
             VALUES (?, X'00', 0, ?, 1, 2)",
            rusqlite::params![id.to_vec(), name],
        )
        .expect("seed streams");
    }

    conn.execute(
        "INSERT INTO tasks (id, stream_id, title, state, created_at_ms, updated_at_ms)
         VALUES (?, ?, ?, 'open', 3, 4)",
        rusqlite::params![
            TASK_IN_INBOX.to_vec(),
            LEGACY_INBOX.to_vec(),
            INBOX_TASK_TITLE
        ],
    )
    .expect("seed the inbox task");
    conn.execute(
        "INSERT INTO tasks (id, stream_id, title, state, body, created_at_ms, updated_at_ms)
         VALUES (?, ?, ?, 'open', ?, 5, 6)",
        rusqlite::params![
            TASK_IN_ALPHA.to_vec(),
            STREAM_ALPHA.to_vec(),
            ALPHA_TASK_TITLE,
            ALPHA_TASK_BODY,
        ],
    )
    .expect("seed the alpha task");

    conn.execute(
        "INSERT INTO contexts (id, name) VALUES (?, 'Errands')",
        rusqlite::params![CONTEXT_ERRANDS.to_vec()],
    )
    .expect("seed contexts");
    conn.execute(
        "INSERT INTO task_contexts (task_id, context_id) VALUES (?, ?)",
        rusqlite::params![TASK_IN_ALPHA.to_vec(), CONTEXT_ERRANDS.to_vec()],
    )
    .expect("seed task_contexts");

    conn.execute(
        "INSERT INTO ops
         (op_id, stream_id, device_id, seq, ts_ms, envelope, inner_kind,
          target_kind, target_id, received_at)
         VALUES (?, ?, ?, 1, 7, ?, 'task_create', 'task', ?, 8)",
        rusqlite::params![
            OP_ID.to_vec(),
            STREAM_ALPHA.to_vec(),
            DEVICE_LAPTOP.to_vec(),
            OP_ENVELOPE,
            TASK_IN_ALPHA.to_vec(),
        ],
    )
    .expect("seed ops");
    conn.execute(
        "INSERT INTO outbox (op_id, stream_id, enqueued_at_ms) VALUES (?, ?, 9)",
        rusqlite::params![OP_ID.to_vec(), STREAM_ALPHA.to_vec()],
    )
    .expect("seed outbox");
    conn.execute(
        "INSERT INTO sync_cursors (stream_id, device_id, last_applied_seq)
         VALUES (?, ?, 1)",
        rusqlite::params![STREAM_ALPHA.to_vec(), DEVICE_LAPTOP.to_vec()],
    )
    .expect("seed sync_cursors");

    seed_v13_devices_and_keys(conn);
}

/// The half of a 13-era vault 0017 rewrites: the local identity, one live and
/// one revoked device, and a `stream_keys` row from the derived schedule.
fn seed_v13_devices_and_keys(conn: &Connection) {
    conn.execute(
        "INSERT INTO local_identity
         (id, device_id, signing_secret_wrapped, cert_blob, created_at_ms)
         VALUES (1, ?, ?, ?, 10)",
        rusqlite::params![
            DEVICE_LAPTOP.to_vec(),
            SIGNING_SECRET_WRAPPED,
            DEVICE_CERT_BLOB,
        ],
    )
    .expect("seed local_identity");
    conn.execute(
        "INSERT INTO devices
         (device_id, cert_blob, nickname, platform, created_at_ms, revoked_at_ms)
         VALUES (?, ?, 'laptop', 'macos', 10, NULL)",
        rusqlite::params![DEVICE_LAPTOP.to_vec(), DEVICE_CERT_BLOB],
    )
    .expect("seed the live device");
    conn.execute(
        "INSERT INTO devices
         (device_id, cert_blob, nickname, platform, created_at_ms, revoked_at_ms)
         VALUES (?, ?, 'old-phone', 'ios', 10, ?)",
        rusqlite::params![DEVICE_RETIRED.to_vec(), DEVICE_CERT_BLOB, RETIRED_AT_MS],
    )
    .expect("seed the retired device");

    conn.execute(
        "INSERT INTO stream_keys (stream_id, epoch, wrapped, created_at_ms)
         VALUES (?, 1, ?, 11)",
        rusqlite::params![STREAM_ALPHA.to_vec(), WRAPPED_STREAM_KEY],
    )
    .expect("seed the pre-hierarchy stream key");
}

/// The rows a 17-era vault holds: the same shape, already past 0017, plus the
/// account identity and the re-keyed `stream_keys` that only exist from 17 on.
///
/// `source = 'local'` on vault-meta epoch 1 makes this the account's
/// **creator**, which is the branch of 0019 that must *not* lose data. The
/// paired branch — where the column is deliberately blanked — is covered by
/// `db.rs`'s `migration_0019_clears_a_paired_devices_identity_key_and_keeps_the_creators`,
/// and cannot share a fixture with this one because `identity` holds exactly
/// one row.
fn seed_v17(conn: &Connection) {
    conn.execute(
        "INSERT INTO streams
         (stream_id, head_root, last_op_seq, name, sort_order, created_at_ms, updated_at_ms)
         VALUES (?, X'00', 0, '', 'AAAN', 1, 2)",
        rusqlite::params![REMAPPED_INBOX.to_vec()],
    )
    .expect("seed the inbox stream");
    conn.execute(
        "INSERT INTO streams
         (stream_id, head_root, last_op_seq, name, sort_order, created_at_ms, updated_at_ms)
         VALUES (?, X'00', 0, 'Alpha', 'AABN', 1, 2)",
        rusqlite::params![STREAM_ALPHA.to_vec()],
    )
    .expect("seed the alpha stream");
    conn.execute(
        "INSERT INTO tasks (id, stream_id, title, state, body, created_at_ms, updated_at_ms)
         VALUES (?, ?, ?, 'open', ?, 5, 6)",
        rusqlite::params![
            TASK_IN_ALPHA.to_vec(),
            STREAM_ALPHA.to_vec(),
            ALPHA_TASK_TITLE,
            ALPHA_TASK_BODY,
        ],
    )
    .expect("seed the alpha task");
    conn.execute(
        "INSERT INTO ops
         (op_id, stream_id, device_id, seq, ts_ms, envelope, inner_kind,
          target_kind, target_id, received_at)
         VALUES (?, ?, ?, 1, 7, ?, 'task_create', 'task', ?, 8)",
        rusqlite::params![
            OP_ID.to_vec(),
            STREAM_ALPHA.to_vec(),
            DEVICE_LAPTOP.to_vec(),
            OP_ENVELOPE,
            TASK_IN_ALPHA.to_vec(),
        ],
    )
    .expect("seed ops");

    conn.execute(
        "INSERT INTO local_identity
         (id, device_id, signing_secret_wrapped, cert_blob, dh_secret_wrapped, created_at_ms)
         VALUES (1, ?, ?, ?, ?, 10)",
        rusqlite::params![
            DEVICE_LAPTOP.to_vec(),
            SIGNING_SECRET_WRAPPED,
            DEVICE_CERT_BLOB,
            DH_SECRET_WRAPPED,
        ],
    )
    .expect("seed local_identity");
    conn.execute(
        "INSERT INTO identity
         (id, identity_id, id_s_pub, id_d_pub, id_s_priv_wrapped, id_d_priv_wrapped,
          created_at_ms)
         VALUES (1, ?, ?, ?, ?, ?, 10)",
        rusqlite::params![
            IDENTITY_ID.to_vec(),
            ID_S_PUB.to_vec(),
            ID_D_PUB.to_vec(),
            ID_S_PRIV_WRAPPED,
            ID_D_PRIV_WRAPPED,
        ],
    )
    .expect("seed identity");
    conn.execute(
        "INSERT INTO devices
         (device_id, cert_blob, nickname, platform, created_at_ms, identity_id, d_d_pub)
         VALUES (?, ?, 'laptop', 'macos', 10, ?, ?)",
        rusqlite::params![
            DEVICE_LAPTOP.to_vec(),
            DEVICE_CERT_BLOB,
            IDENTITY_ID.to_vec(),
            ID_D_PUB.to_vec(),
        ],
    )
    .expect("seed devices");

    for (stream, key_id, source) in [
        (VAULT_META_STREAM, META_KEY_ID, "local"),
        (STREAM_ALPHA, ALPHA_KEY_ID, "envelope"),
    ] {
        conn.execute(
            "INSERT INTO stream_keys
             (stream_id, epoch, key_id, wrapped, source, created_at_ms)
             VALUES (?, 1, ?, ?, ?, 11)",
            rusqlite::params![stream.to_vec(), key_id, WRAPPED_STREAM_KEY, source],
        )
        .expect("seed stream_keys");
    }
}

// --- the generator --------------------------------------------------------

/// Write one fixture: a vault at `storage_v`, seeded, checkpointed, single-file.
fn write_fixture(name: &str, storage_v: u32, seed: fn(&Connection)) {
    let dir = fixture_dir();
    fs::create_dir_all(&dir).expect("create the fixture directory");
    let path = dir.join(name);
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(dir.join(format!("{name}{suffix}")));
    }

    let db = Db::create_at_storage_v(&path, &fixture_key(), storage_v)
        .expect("create the fixture vault");
    seed(db.conn());
    assert_eq!(
        stamped_storage_v(&db),
        storage_v,
        "the stamp is the fixture"
    );
    // WAL leaves `-wal`/`-shm` beside the database. A fixture is one file or
    // it is not a fixture: half of it would be uncommitted and the committed
    // half would be missing every row.
    db.conn()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .expect("checkpoint");
    drop(db);
    for suffix in ["-wal", "-shm"] {
        let sidecar = dir.join(format!("{name}{suffix}"));
        assert!(
            !sidecar.exists() || fs::metadata(&sidecar).expect("stat").len() == 0,
            "`{}` still holds data; the fixture is incomplete",
            sidecar.display()
        );
        let _ = fs::remove_file(&sidecar);
    }
}

/// Rewrite every committed fixture from the seeds above.
///
/// `#[ignore]`d: an ordinary `cargo test` must read the committed files, not
/// replace them. Run it through `mise run storage-fixtures`.
#[test]
#[ignore = "rewrites the committed fixtures; run it through `mise run storage-fixtures`"]
fn regenerate_the_committed_vault_fixtures() {
    write_fixture(V13_FIXTURE, 13, seed_v13);
    write_fixture(V17_FIXTURE, 17, seed_v17);
}

// --- what the chain must preserve, from 13 --------------------------------

/// The committed file really is old, and really is encrypted.
///
/// Asserted before anything else because every test below is worthless
/// otherwise: a fixture accidentally regenerated at the current version would
/// let all of them pass while exercising no migration at all.
#[test]
fn the_committed_v13_fixture_is_a_sealed_vault_stamped_at_the_baseline() {
    let bytes = fs::read(fixture_dir().join(V13_FIXTURE)).expect("read the fixture");
    assert!(
        !bytes.starts_with(b"SQLite format 3\0"),
        "the fixture must be encrypted, or it is not a vault"
    );
    assert!(
        !bytes
            .windows(INBOX_TASK_TITLE.len())
            .any(|w| w == INBOX_TASK_TITLE.as_bytes()),
        "the fixture holds its content in the clear"
    );

    // Opened without migrating, by keying it and reading the stamp.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(V13_FIXTURE);
    fs::copy(fixture_dir().join(V13_FIXTURE), &path).expect("copy");
    let raw = Db::open_unmigrated(&path, &fixture_key()).expect("key the existing file");
    assert_eq!(
        stamped_storage_v(&raw),
        13,
        "the fixture must still be a 13-era vault; regenerating it at the \
         current version would silently disable every test in this module"
    );
}

/// The chain runs, and lands on this build's version.
#[test]
fn a_v13_vault_migrates_all_the_way_forward() {
    let (_dir, db) = open_migrated(V13_FIXTURE);
    assert_eq!(stamped_storage_v(&db), u32::from(STORAGE_V));
    for table in [
        "identity",
        "deferred_ops",
        "device_revocations",
        "key_envelope_recipients",
        "relay_revocation_intents",
    ] {
        assert!(
            table_exists(&db, table),
            "`{table}` must exist after 13 -> 20"
        );
    }
}

/// 0014's backfill, over rows that were written before it existed.
///
/// The unit test in `db.rs` replays 0013 then 0014 into a bare connection; this
/// is the same property through `Db::open` on a real file, and it is the only
/// place the backfill is observed after the *rest* of the chain has also run
/// over it. The keys are exact rather than merely ordered: 0014's contract is
/// that a user's sidebar does not rearrange itself, so "some ascending keys"
/// is not the promise.
#[test]
fn a_v13_vault_keeps_its_streams_and_gains_the_sort_keys_0014_backfills() {
    let (_dir, db) = open_migrated(V13_FIXTURE);
    let mut stmt = db
        .conn()
        .prepare("SELECT name, sort_order FROM streams ORDER BY sort_order, name")
        .expect("prepare");
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    drop(stmt);

    assert_eq!(
        rows,
        vec![
            // 0017's Inbox row is created after 0014 has run, so it takes the
            // "never ordered" sentinel rather than a backfilled key.
            (String::new(), String::new()),
            (String::new(), "AAAN".to_string()),
            ("Alpha".to_string(), "AABN".to_string()),
            ("beta".to_string(), "AACN".to_string()),
            ("Gamma".to_string(), "AADN".to_string()),
        ],
        "0014 assigns keys in the pre-0014 display order (name COLLATE NOCASE)"
    );
}

/// The task rows survive, and 0017 re-points the one parked in the legacy
/// Inbox rather than orphaning or dropping it.
#[test]
fn a_v13_vault_keeps_its_tasks_and_0017_repoints_the_legacy_inbox() {
    let (_dir, db) = open_migrated(V13_FIXTURE);
    assert_eq!(row_count(&db, "tasks"), 2, "no task may be lost");

    let (stream, title): (Vec<u8>, String) = db
        .conn()
        .query_row(
            "SELECT stream_id, title FROM tasks WHERE id = ?",
            rusqlite::params![TASK_IN_INBOX.to_vec()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("the inbox task");
    assert_eq!(
        stream,
        REMAPPED_INBOX.to_vec(),
        "0017 moves the Inbox off the vault-meta stream id"
    );
    assert_eq!(
        title, INBOX_TASK_TITLE,
        "and does not touch the row's content"
    );

    let (stream, title, body): (Vec<u8>, String, Vec<u8>) = db
        .conn()
        .query_row(
            "SELECT stream_id, title, body FROM tasks WHERE id = ?",
            rusqlite::params![TASK_IN_ALPHA.to_vec()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("the alpha task");
    assert_eq!(stream, STREAM_ALPHA.to_vec());
    assert_eq!(title, ALPHA_TASK_TITLE);
    assert_eq!(
        body, ALPHA_TASK_BODY,
        "a NoteBody is not the migrator's to rewrite"
    );

    // The task's context membership rode through with it.
    let context: Vec<u8> = db
        .conn()
        .query_row(
            "SELECT context_id FROM task_contexts WHERE task_id = ?",
            rusqlite::params![TASK_IN_ALPHA.to_vec()],
            |r| r.get(0),
        )
        .expect("the membership row");
    assert_eq!(context, CONTEXT_ERRANDS.to_vec());
}

/// The op log is the one thing that cannot be re-derived, and its envelopes
/// are signed: not one byte may differ after seven migrations.
#[test]
fn a_v13_vaults_op_log_comes_through_byte_for_byte() {
    let (_dir, db) = open_migrated(V13_FIXTURE);
    let (stream, device, envelope, kind): (Vec<u8>, Vec<u8>, Vec<u8>, String) = db
        .conn()
        .query_row(
            "SELECT stream_id, device_id, envelope, inner_kind FROM ops WHERE op_id = ?",
            rusqlite::params![OP_ID.to_vec()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("the seeded op");
    assert_eq!(envelope, OP_ENVELOPE, "the signature covers these bytes");
    assert_eq!(stream, STREAM_ALPHA.to_vec());
    assert_eq!(device, DEVICE_LAPTOP.to_vec());
    assert_eq!(kind, "task_create");

    // 0017 re-points `tasks.stream_id` and deliberately does NOT rewrite the
    // envelope that named the old id -- it cannot, the signature covers it.
    // `Engine::remap_legacy_inbox` is what reconciles the two on replay, and
    // this pins the half the migration is responsible for.
    assert_eq!(row_count(&db, "outbox"), 1, "the unacked push survives");
    assert_eq!(row_count(&db, "sync_cursors"), 1, "so does the pull cursor");
}

/// 0017's two device moves: the revocation is carried into its own table with
/// a zero HLC, and the superseded column is cleared without losing the device.
#[test]
fn a_v13_vaults_revoked_device_becomes_a_device_revocations_row() {
    let (_dir, db) = open_migrated(V13_FIXTURE);
    assert_eq!(row_count(&db, "devices"), 2, "no device row may be dropped");
    let still_stamped: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM devices WHERE revoked_at_ms IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(
        still_stamped, 0,
        "`devices.revoked_at_ms` is superseded by `device_revocations`"
    );

    let (device, cut_ms, cut_logical, reason): (Vec<u8>, i64, i64, String) = db
        .conn()
        .query_row(
            "SELECT device_id, cut_ms, cut_logical, reason FROM device_revocations",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("exactly one revocation");
    assert_eq!(device, DEVICE_RETIRED.to_vec());
    assert_eq!(cut_ms, RETIRED_AT_MS, "the cut is the old stamp, not now");
    assert_eq!(
        cut_logical, 0,
        "a zero HLC, so the first real device_revoke op supersedes it"
    );
    assert_eq!(reason, "Retired");
}

/// The one deliberate loss in the chain, asserted as a loss.
///
/// 0017 drops every pre-hierarchy `stream_keys` row because each wrapped a key
/// *derived* from the vault root, which `Keychain::open` recomputes on the next
/// open. It is pinned here so that "the fixture survives migration" cannot be
/// read as "nothing is ever dropped": one thing is, on purpose, and this is it.
#[test]
fn a_v13_vaults_derived_stream_keys_are_dropped_and_the_local_identity_is_not() {
    let (_dir, db) = open_migrated(V13_FIXTURE);
    assert_eq!(
        row_count(&db, "stream_keys"),
        0,
        "derived pre-0017 keys are re-derived on open, not migrated"
    );

    let (device, secret, cert, dh): LocalIdentityRow = db
        .conn()
        .query_row(
            "SELECT device_id, signing_secret_wrapped, cert_blob, dh_secret_wrapped
             FROM local_identity WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("the local identity");
    assert_eq!(device, DEVICE_LAPTOP.to_vec());
    assert_eq!(
        secret, SIGNING_SECRET_WRAPPED,
        "the device signing seed exists in exactly one place"
    );
    assert_eq!(cert, DEVICE_CERT_BLOB);
    assert!(
        dh.is_none(),
        "0017 adds the column nullable; `adopt_legacy_vault` fills it on open"
    );

    // 0019 has nothing to do on a vault that started at 13: 0017 created
    // `identity` empty. That is the gap the v17 fixture below exists to close.
    assert_eq!(row_count(&db, "identity"), 0);
}

/// 0015 and 0016 are additive, and additive has to mean the existing rows are
/// still there afterwards with their new columns empty.
#[test]
fn a_v13_vaults_rows_gain_0015_and_0016_columns_holding_null() {
    let (_dir, db) = open_migrated(V13_FIXTURE);
    let (name, extra, description, default_context): StreamAdditiveColumns = db
        .conn()
        .query_row(
            "SELECT name, extra, description, default_context FROM streams
             WHERE stream_id = ?",
            rusqlite::params![STREAM_ALPHA.to_vec()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("the alpha stream");
    assert_eq!(name, "Alpha");
    assert!(extra.is_none(), "NULL is what a pre-0015 row meant");
    assert!(description.is_none());
    assert!(default_context.is_none());

    let (context_name, context_extra): (String, Option<Vec<u8>>) = db
        .conn()
        .query_row(
            "SELECT name, extra FROM contexts WHERE id = ?",
            rusqlite::params![CONTEXT_ERRANDS.to_vec()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("the context");
    assert_eq!(context_name, "Errands");
    assert!(context_extra.is_none());
}

// --- what the chain must preserve, from 17 --------------------------------

/// The second fixture is what makes 0019 testable at all against a real vault.
///
/// 0019 blanks `identity.id_d_priv_wrapped` on every vault it cannot prove
/// minted the account, and on the creator that column holds the only copy of
/// `ID_D_priv` in existence — no code path writes a recovery blob. A migration
/// that ran clean and blanked it would pass any "does the vault open" test and
/// would have destroyed the account.
#[test]
fn a_v17_creator_vault_keeps_the_only_copy_of_its_identity_key() {
    let (_dir, db) = open_migrated(V17_FIXTURE);
    assert_eq!(stamped_storage_v(&db), u32::from(STORAGE_V));

    let (identity_id, s_pub, d_pub, s_priv, d_priv, minted_by): IdentityRow = db
        .conn()
        .query_row(
            "SELECT identity_id, id_s_pub, id_d_pub, id_s_priv_wrapped,
                    id_d_priv_wrapped, minted_by_device_id
             FROM identity WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .expect("the identity row");
    assert_eq!(identity_id, IDENTITY_ID.to_vec());
    assert_eq!(s_pub, ID_S_PUB.to_vec());
    assert_eq!(d_pub, ID_D_PUB.to_vec());
    assert_eq!(s_priv, ID_S_PRIV_WRAPPED);
    assert_eq!(
        d_priv, ID_D_PRIV_WRAPPED,
        "vault-meta epoch 1 with source 'local' is proof this device minted \
         the identity, so 0019 must leave its key alone"
    );
    assert_eq!(
        minted_by,
        Some(DEVICE_LAPTOP.to_vec()),
        "and 0019 records the fact rather than leaving it to be re-inferred"
    );
}

/// 0017-era stream keys are the schedule 0018-0020 were written for, so unlike
/// the derived ones they must all survive, provenance included.
#[test]
fn a_v17_vault_keeps_its_wrapped_stream_keys_and_their_provenance() {
    let (_dir, db) = open_migrated(V17_FIXTURE);
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT stream_id, key_id, wrapped, source FROM stream_keys
             ORDER BY source",
        )
        .expect("prepare");
    let rows: Vec<StreamKeyRow> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    drop(stmt);
    assert_eq!(
        rows,
        vec![
            (
                STREAM_ALPHA.to_vec(),
                ALPHA_KEY_ID.to_vec(),
                WRAPPED_STREAM_KEY.to_vec(),
                "envelope".to_string(),
            ),
            (
                VAULT_META_STREAM.to_vec(),
                META_KEY_ID.to_vec(),
                WRAPPED_STREAM_KEY.to_vec(),
                "local".to_string(),
            ),
        ],
        "an independently random per-epoch key cannot be re-derived, so losing \
         one loses every op sealed under it"
    );

    let dh: Option<Vec<u8>> = db
        .conn()
        .query_row(
            "SELECT dh_secret_wrapped FROM local_identity WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .expect("the local identity");
    assert_eq!(
        dh.as_deref(),
        Some(DH_SECRET_WRAPPED),
        "the device X25519 secret is what every key_envelope is sealed to"
    );
}

/// 0018 and 0020 add tables and say in terms that they arrive empty. Asserted,
/// because "empty" is a decision here rather than an omission: a reconstructed
/// recipient index would need the stream keys, and a queue of relay calls
/// invented for devices revoked before 0020 would call the relay about them.
#[test]
fn a_v17_vault_gains_the_0018_and_0020_tables_empty() {
    let (_dir, db) = open_migrated(V17_FIXTURE);
    for table in ["key_envelope_recipients", "relay_revocation_intents"] {
        assert!(
            table_exists(&db, table),
            "`{table}` must exist after 17 -> 20"
        );
        assert_eq!(row_count(&db, table), 0, "`{table}` arrives empty");
    }
    // ...and the op log is untouched by any of it.
    let envelope: Vec<u8> = db
        .conn()
        .query_row(
            "SELECT envelope FROM ops WHERE op_id = ?",
            rusqlite::params![OP_ID.to_vec()],
            |r| r.get(0),
        )
        .expect("the seeded op");
    assert_eq!(envelope, OP_ENVELOPE);
}

/// Re-opening a migrated fixture is a no-op, which is the property
/// `docs/04-storage/migrations.md` attributes to the runner rather than to the
/// SQL. Worth pinning here and not only in `tests/vault_at_rest.rs`, because
/// this is the one place a vault reaches the current version by *migrating*
/// rather than by being created there.
#[test]
fn re_opening_a_migrated_fixture_changes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(V13_FIXTURE);
    fs::copy(fixture_dir().join(V13_FIXTURE), &path).expect("copy");

    let fingerprint = |db: &Db| -> String {
        let mut stmt = db
            .conn()
            .prepare("SELECT type, name, COALESCE(sql, '') FROM sqlite_master ORDER BY type, name")
            .expect("prepare");
        let rows: Vec<String> = stmt
            .query_map([], |r| {
                Ok(format!(
                    "{}\t{}\t{}",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?
                ))
            })
            .expect("query")
            .map(|r| r.expect("row"))
            .collect();
        rows.join("\n")
    };

    let first = {
        let db = Db::open(&path, &fixture_key()).expect("migrate the fixture");
        fingerprint(&db)
    };
    for open in 2..=3 {
        let db = Db::open(&path, &fixture_key()).unwrap_or_else(|e| panic!("open {open}: {e}"));
        assert_eq!(
            fingerprint(&db),
            first,
            "open {open} re-applied a migration"
        );
        assert_eq!(stamped_storage_v(&db), u32::from(STORAGE_V));
        assert_eq!(row_count(&db, "tasks"), 2, "open {open} lost a row");
    }
}

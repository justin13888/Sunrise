//! What the chain must preserve, from 13.

use std::fs;

use sunrise_cbor::version::STORAGE_V;

use super::contents::{
    ALPHA_TASK_BODY, ALPHA_TASK_TITLE, CONTEXT_ERRANDS, DEVICE_CERT_BLOB, DEVICE_LAPTOP,
    DEVICE_RETIRED, INBOX_TASK_TITLE, OP_ENVELOPE, OP_ID, RETIRED_AT_MS, SIGNING_SECRET_WRAPPED,
    STREAM_ALPHA, TASK_IN_ALPHA, TASK_IN_INBOX,
};
use super::{
    fixture_dir, fixture_key, open_migrated, row_count, stamped_storage_v, table_exists,
    V13_FIXTURE,
};
use crate::db::Db;

/// A `streams` row's `(name, extra, description, default_context)`: the name
/// 0013 wrote and the three columns 0015 and 0016 add beside it.
type StreamAdditiveColumns = (String, Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>);

/// A `local_identity` row's
/// `(device_id, signing_secret_wrapped, cert_blob, dh_secret_wrapped)`.
type LocalIdentityRow = (Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);

/// The committed file really is old, and really is encrypted.
///
/// Asserted because every test over this file is worthless
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
        // The other revocation table. Listed beside the register because the
        // two answer different questions and a vault that arrived with only
        // one of them would run the four key-distribution anti-joins against a
        // table that does not exist.
        "device_read_bounds",
        // Read beside `device_read_bounds` by `Keychain::current_epoch_tx`,
        // which would fail on every write without it.
        "stream_key_senders",
        "key_envelope_recipients",
        "relay_revocation_intents",
        // The parked-op marker. Without it every delivery of a kind this build
        // does not know fails its transaction, and the op is refused rather
        // than kept.
        "parked_ops",
    ] {
        assert!(
            table_exists(&db, table),
            "`{table}` must exist after the whole chain has run"
        );
    }
}

/// 0014's backfill, over rows that were written before it existed.
///
/// The unit test in `db/tests.rs` replays 0013 then 0014 into a bare connection; this
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
            // 0017's Inbox row, created after 0014 had run, is gone: 0044
            // deletes it, since the private domain has no stream row.
            (String::new(), "AAAN".to_string()),
            ("Alpha".to_string(), "AABN".to_string()),
            ("beta".to_string(), "AACN".to_string()),
            ("Gamma".to_string(), "AADN".to_string()),
        ],
        "0014 assigns keys in the pre-0014 display order (name COLLATE NOCASE)"
    );
}

/// The task rows survive, and 0017 re-points the one parked in the legacy
/// Inbox rather than orphaning or dropping it, which 0044 then reads as no
/// stream (ADR-0046 §5).
#[test]
fn a_v13_vault_keeps_its_tasks_and_0017_repoints_the_legacy_inbox() {
    let (_dir, db) = open_migrated(V13_FIXTURE);
    assert_eq!(row_count(&db, "tasks"), 2, "no task may be lost");

    let (stream, title): (Option<Vec<u8>>, String) = db
        .conn()
        .query_row(
            "SELECT stream_id, title FROM tasks WHERE id = ?",
            rusqlite::params![TASK_IN_INBOX.to_vec()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("the inbox task");
    assert_eq!(
        stream, None,
        "0017 moves the Inbox off the vault-meta stream id, and 0044 reads it as no stream"
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

    // `query_row` reads the first row and ignores the rest, so the count is
    // what establishes "exactly one".
    assert_eq!(
        row_count(&db, "device_revocations"),
        1,
        "the one revoked device becomes exactly one revocation"
    );
    let (device, cut_ms, cut_logical, reason): (Vec<u8>, i64, i64, String) = db
        .conn()
        .query_row(
            "SELECT device_id, cut_ms, cut_logical, reason FROM device_revocations",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("read the revocation");
    assert_eq!(device, DEVICE_RETIRED.to_vec());
    assert_eq!(cut_ms, RETIRED_AT_MS, "the cut is the old stamp, not now");
    assert_eq!(
        cut_logical, 0,
        "a zero HLC, so the first real device_revoke op supersedes it"
    );
    assert_eq!(reason, "Retired");
}

/// 0027 seeds its ledger from the register, so an upgraded vault folds *from*
/// what it already holds rather than from an empty ledger.
///
/// Not necessarily *to* what it already holds. What the seed preserves is the
/// fold's input and not its output: a seeded register holding a chain of
/// revocations folds to less than it held, which
/// `crates/sunrise-storage/migrations/0027_device_revoke_ops.sql` traces and
/// `the_0027_seed_carries_a_chain_of_revocations_into_the_ledger` pins. This
/// fixture's register holds one row, so here the two coincide.
///
/// The seeded sender is the empty blob — a pre-0017 `devices.revoked_at_ms`
/// names nobody — and that is asserted rather than tolerated: the fold gates on
/// "is this sender a revoked device", the empty blob matches no device id, and
/// inventing one here would be the migration claiming a fact the vault does not
/// have.
#[test]
fn a_v13_vaults_revocation_is_seeded_into_the_0027_ledger() {
    let (_dir, db) = open_migrated(V13_FIXTURE);
    assert!(table_exists(&db, "device_revoke_ops"));
    // `query_row` reads the first row and ignores the rest, so the count is
    // what establishes "exactly one".
    assert_eq!(
        row_count(&db, "device_revoke_ops"),
        1,
        "the one revocation seeds exactly one ledger row"
    );
    let (sender, revoked, ms, logical): (Vec<u8>, Vec<u8>, i64, i64) = db
        .conn()
        .query_row(
            "SELECT sender, revoked_device_id, op_hlc_ms, op_hlc_logical
             FROM device_revoke_ops",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("read the ledger row");
    assert!(
        sender.is_empty(),
        "a carried-over revocation names no sender"
    );
    assert_eq!(revoked, DEVICE_RETIRED.to_vec());
    assert_eq!(ms, RETIRED_AT_MS);
    assert_eq!(logical, 0);
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
    // `identity` empty. That is the gap the v17 fixture exists to close.
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

/// The v13 case, which is the *absence* the `vault_fixtures` header claims: 0022's
/// `UPDATE` matches no row, because 0017 created `identity` empty and nothing
/// since has filled it. The table still has to appear, and the column with it.
#[test]
fn a_v13_vault_gains_the_0022_schema_with_no_identity_to_anchor() {
    let (_dir, db) = open_migrated(V13_FIXTURE);
    assert_eq!(
        row_count(&db, "identity"),
        0,
        "0017 creates `identity` empty; `Keychain::adopt_legacy_vault` fills \
         it on the next open, which is not a migration"
    );
    assert!(table_exists(&db, "identity_transitions"));
    assert_eq!(row_count(&db, "identity_transitions"), 0);
    // The column exists even with no row to carry it, so the adoption path
    // that mints the identity afterwards has somewhere to write the genesis.
    let columns: i64 = db
        .conn()
        .query_row(
            "SELECT count(*) FROM pragma_table_info('identity') \
             WHERE name IN ('genesis_identity_id', 'genesis_id_s_pub')",
            [],
            |r| r.get(0),
        )
        .expect("pragma");
    assert_eq!(columns, 2);
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

//! What the chain must preserve, from 17.

use std::fs;

use sunrise_cbor::version::STORAGE_V;

use super::contents::{
    ALPHA_KEY_ID, DEVICE_LAPTOP, DH_SECRET_WRAPPED, IDENTITY_ID, ID_D_PRIV_WRAPPED, ID_D_PUB,
    ID_S_PRIV_WRAPPED, ID_S_PUB, META_KEY_ID, OP_ENVELOPE, OP_ID, RETIRED_AT_MS, STREAM_ALPHA,
    VAULT_META_STREAM, WRAPPED_STREAM_KEY,
};
use super::{
    fixture_dir, fixture_key, open_migrated, row_count, stamped_storage_v, table_exists,
    V17_FIXTURE,
};
use crate::db::Db;

/// The whole `identity` row, in the order 0019 decides its fate:
/// `(identity_id, id_s_pub, id_d_pub, id_s_priv_wrapped, id_d_priv_wrapped,
/// minted_by_device_id)`.
type IdentityRow = (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, Option<Vec<u8>>);

/// A `stream_keys` row's `(stream_id, key_id, wrapped, source)`.
type StreamKeyRow = (Vec<u8>, Vec<u8>, Vec<u8>, String);

/// **Two revocations sharing a cut do not stop the vault opening.**
///
/// 0027's seed is a bare `INSERT ... SELECT` out of `device_revocations`, and
/// `device_revocations` is keyed on `device_id` alone — so two of its rows may
/// legitimately agree on `(cut_ms, cut_logical, revoked_by)`. Two ways in, both
/// reachable: a peer emitting two `device_revoke` ops at one HLC before this
/// vault upgrades, and 0017's own backfill, which gives every pre-0017
/// `devices.revoked_at_ms` a logical of 0 under an empty `revoked_by` and so
/// collides whenever two devices were retired in the same millisecond.
///
/// Keyed on `(op_hlc_ms, op_hlc_logical, sender)` the seed raised
/// `SQLITE_CONSTRAINT`, `run_migrations` returned `DbError::Migration`, and
/// **the vault did not open** — with migrations forward-only and no backout, a
/// restore from backup replays the same failure. `revoked_device_id` in the key
/// is what makes the seeded rows distinct by construction, and this is the
/// assertion that says so about a real file rather than about the DDL.
///
/// The colliding rows are built into a temporary copy at run time. The
/// committed fixture is not touched: regenerating one at the current version
/// would silently disable every test in this module, which is what
/// `the_committed_v13_fixture_is_a_sealed_vault_stamped_at_the_baseline`
/// exists to catch.
#[test]
fn a_v17_vault_whose_revocations_share_a_cut_still_opens() {
    /// Two devices retired in one millisecond by the same party, which is all
    /// it takes.
    const FIRST_RETIRED: [u8; 16] = [0xe1; 16];
    const SECOND_RETIRED: [u8; 16] = [0xe2; 16];

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(V17_FIXTURE);
    fs::copy(fixture_dir().join(V17_FIXTURE), &path).expect("copy the v17 fixture");
    {
        let raw = Db::open_unmigrated(&path, &fixture_key()).expect("key the existing file");
        for device in [FIRST_RETIRED, SECOND_RETIRED] {
            raw.conn()
                .execute(
                    "INSERT INTO device_revocations
                     (device_id, cut_ms, cut_logical, revoked_by, reason, recorded_at_ms)
                     VALUES (?, ?, 0, ?, 'Retired', ?)",
                    rusqlite::params![
                        device.to_vec(),
                        RETIRED_AT_MS,
                        DEVICE_LAPTOP.to_vec(),
                        RETIRED_AT_MS,
                    ],
                )
                .expect("seed a colliding revocation");
        }
        raw.conn()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .expect("checkpoint");
    }

    let db = Db::open(&path, &fixture_key())
        .expect("two revocations at one cut must not stop the migration chain");
    assert_eq!(stamped_storage_v(&db), u32::from(STORAGE_V));
    assert_eq!(
        row_count(&db, "device_revoke_ops"),
        2,
        "both revocations are seeded into the ledger; neither is dropped"
    );
    let targets: Vec<Vec<u8>> = {
        let conn = db.conn();
        let mut stmt = conn
            .prepare("SELECT revoked_device_id FROM device_revoke_ops ORDER BY revoked_device_id")
            .expect("prepare");
        let rows = stmt
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect");
        rows
    };
    assert_eq!(
        targets,
        vec![FIRST_RETIRED.to_vec(), SECOND_RETIRED.to_vec()],
        "the two rows are told apart by the device each names"
    );
}

/// **A chained register is seeded as a chain, and that is the case the seed's
/// own reasoning turns on.**
///
/// `crates/sunrise-storage/migrations/0027_device_revoke_ops.sql` argues that
/// what the seed preserves is the fold's **input** and not its output, and its
/// worked trace is a register holding a chain: `{C revoked by A, A revoked by
/// B}` seeds `A -> C` and `B -> A`, and at the next `device_revoke` the fold
/// gates `A -> C` because `A`'s revoker set is `{B}` and `B` is not `C`, so C
/// leaves the register. That argument is about *these two ledger rows*, and
/// until this test nothing said they are what the seed produces — a seed that
/// collapsed a chain, dropped the second row or lost a sender would have left
/// the comment reasoning about a shape the migration no longer emits.
///
/// Only the seed is asserted here: the fold is `sunrise-core`'s and is pinned
/// on its own side by
/// `a_revocation_written_before_the_senders_own_cut_is_unwound_when_the_sender_is_revoked`.
/// What this owns is that the migration hands that fold a chain.
///
/// Built into a temporary copy at run time, for the reason
/// `a_v17_vault_whose_revocations_share_a_cut_still_opens` gives: the committed
/// fixture is not touched.
#[test]
fn the_0027_seed_carries_a_chain_of_revocations_into_the_ledger() {
    /// `{C revoked by A, A revoked by B}` — retire a laptop from the desktop,
    /// and months later retire the desktop from the phone.
    const CHAIN_C: [u8; 16] = [0xc1; 16];
    const CHAIN_A: [u8; 16] = [0xa1; 16];
    const CHAIN_B: [u8; 16] = [0xb1; 16];

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(V17_FIXTURE);
    fs::copy(fixture_dir().join(V17_FIXTURE), &path).expect("copy the v17 fixture");
    {
        let raw = Db::open_unmigrated(&path, &fixture_key()).expect("key the existing file");
        for (revoked, revoker, cut) in [
            (CHAIN_C, CHAIN_A, RETIRED_AT_MS),
            (CHAIN_A, CHAIN_B, RETIRED_AT_MS + 1),
        ] {
            raw.conn()
                .execute(
                    "INSERT INTO device_revocations
                     (device_id, cut_ms, cut_logical, revoked_by, reason, recorded_at_ms)
                     VALUES (?, ?, 0, ?, 'Retired', ?)",
                    rusqlite::params![revoked.to_vec(), cut, revoker.to_vec(), cut],
                )
                .expect("seed a link of the chain");
        }
        raw.conn()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .expect("checkpoint");
    }

    let db = Db::open(&path, &fixture_key()).expect("a chained register must still migrate");
    assert_eq!(stamped_storage_v(&db), u32::from(STORAGE_V));

    let pairs: Vec<(Vec<u8>, Vec<u8>)> = {
        let conn = db.conn();
        let mut stmt = conn
            .prepare(
                "SELECT sender, revoked_device_id FROM device_revoke_ops
                 ORDER BY revoked_device_id",
            )
            .expect("prepare");
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect");
        rows
    };
    assert_eq!(
        pairs,
        vec![
            (CHAIN_B.to_vec(), CHAIN_A.to_vec()),
            (CHAIN_A.to_vec(), CHAIN_C.to_vec()),
        ],
        "the seed hands the fold `B -> A` and `A -> C`: two rows, each keeping \
         the sender that makes it a link"
    );

    // **And 0028 takes the read bound off the same register, before 0027's
    // fold has had a chance to gate anything.**
    //
    // This is the claim every upgrading user's read bound rests on — that the
    // seed is exact for a vault coming from 26 or below — and it is exactly
    // this fixture that can observe it, because the register it carries is the
    // chain that the first fold afterwards will collapse. C is the interesting
    // one: `A -> C` is gated the moment the fold runs, so a bound derived from
    // the ledger rather than seeded here would never name C at all, and the
    // laptop this user retired first would quietly become a key recipient again
    // on the upgrade.
    let bounded: Vec<Vec<u8>> = {
        let conn = db.conn();
        let mut stmt = conn
            .prepare("SELECT device_id FROM device_read_bounds ORDER BY device_id")
            .expect("prepare");
        let rows = stmt
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect");
        rows
    };
    assert_eq!(
        bounded,
        vec![CHAIN_A.to_vec(), CHAIN_C.to_vec()],
        "0028 seeds the bound from the register it finds, so both links of the \
         chain are read-bounded across the upgrade -- including the one the \
         first fold afterwards stops calling revoked"
    );
}

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

/// 0022 turns the account identity from a value into a chain, and the chain is
/// folded from a fixed point rather than from `identity.identity_id`, which
/// moves on every transition. A vault that predates rotation has taken none, so
/// the identity it holds *is* its genesis — asserted, because the backfill is
/// the only thing that makes that true and leaving the column NULL would open
/// perfectly while leaving every later fold anchored on nothing.
#[test]
fn a_v17_vaults_identity_becomes_its_own_genesis() {
    let (_dir, db) = open_migrated(V17_FIXTURE);
    let (identity_id, genesis): (Vec<u8>, Option<Vec<u8>>) = db
        .conn()
        .query_row(
            "SELECT identity_id, genesis_identity_id FROM identity WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("the identity row");
    assert_eq!(identity_id, IDENTITY_ID.to_vec());
    assert_eq!(
        genesis,
        Some(IDENTITY_ID.to_vec()),
        "a vault that has never rotated is its own genesis, and 0022 must say \
         so rather than leaving the anchor NULL"
    );

    // 0023's half of the same anchor. Without the key, a vault that rotated
    // once could never verify a cert issued before it did: `identity_id` is a
    // one-way derivation of `ID_S_pub`, and `identity.id_s_pub` moves to the
    // successor on the first rotation.
    let genesis_pub: Option<Vec<u8>> = db
        .conn()
        .query_row(
            "SELECT genesis_id_s_pub FROM identity WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .expect("the identity row");
    assert_eq!(genesis_pub, Some(ID_S_PUB.to_vec()));

    // The chain itself arrives empty, and must: a transition is an op, and
    // inventing one for a vault that never emitted it would make every replica
    // fold a different history.
    assert!(table_exists(&db, "identity_transitions"));
    assert_eq!(row_count(&db, "identity_transitions"), 0);
}

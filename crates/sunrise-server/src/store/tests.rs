//! The store's account, device, and handle suite, run against an in-memory
//! database.

use super::*;

fn subject(sub: &str) -> Subject {
    Subject::new("https://idp.example", sub)
}

fn store() -> Store {
    Store::open(None).unwrap()
}

const NOW: u64 = 1_704_067_200_000;

#[test]
fn a_returning_subject_gets_the_same_account() {
    let s = store();
    let a = s.resolve_account(&subject("alice"), true, NOW).unwrap();
    let b = s
        .resolve_account(&subject("alice"), true, NOW + 5000)
        .unwrap();
    assert_eq!(a.account_id, b.account_id);
    assert_eq!(a.created_at_ms, b.created_at_ms);
}

/// The `(iss, sub)` pair is the key, so the same `sub` at a different
/// issuer is a different person.
#[test]
fn the_same_sub_at_another_issuer_is_another_account() {
    let s = store();
    let a = s.resolve_account(&subject("alice"), true, NOW).unwrap();
    let b = s
        .resolve_account(&Subject::new("https://other.example", "alice"), true, NOW)
        .unwrap();
    assert_ne!(a.account_id, b.account_id);
}

#[test]
fn signup_disabled_refuses_an_unknown_subject_but_not_a_known_one() {
    let s = store();
    assert!(matches!(
        s.resolve_account(&subject("alice"), false, NOW),
        Err(StoreError::SignupDisabled)
    ));
    let a = s.resolve_account(&subject("alice"), true, NOW).unwrap();
    // Now that the account exists, the guard no longer applies: it gates
    // provisioning, not login.
    let b = s.resolve_account(&subject("alice"), false, NOW).unwrap();
    assert_eq!(a.account_id, b.account_id);
}

#[test]
fn account_ids_are_not_derived_from_anything_the_caller_supplies() {
    let s = store();
    let a = s.resolve_account(&subject("alice"), true, NOW).unwrap();
    assert_eq!(a.account_id.len(), sunrise_id::crockford::ENCODED_LEN);
    assert!(
        !a.account_id.contains("alice"),
        "the account id must not encode the OIDC subject"
    );
}

#[test]
fn revocation_hides_the_device_from_the_active_lookup_and_drops_push_tokens() {
    let s = store();
    let acct = s.resolve_account(&subject("alice"), true, NOW).unwrap();
    let d = s
        .register_device(
            &acct.account_id,
            &NewDevice {
                device_pub_s: "k".into(),
                device_pub_d: None,
                device_cert: None,
                vault_device_id: None,
                nickname: "laptop".into(),
                platform: "linux".into(),
                app_version: None,
            },
            NOW,
        )
        .unwrap();
    s.upsert_push_token(&d.device_id, "fcm", "tok", NOW)
        .unwrap();
    assert_eq!(s.push_tokens(&d.device_id).unwrap().len(), 1);
    assert_eq!(s.active_device_count(&acct.account_id).unwrap(), 1);

    s.revoke_device(&acct.account_id, &d.device_id, NOW + 1)
        .unwrap();

    assert!(s
        .active_device(&acct.account_id, &d.device_id)
        .unwrap()
        .is_none());
    assert_eq!(s.active_device_count(&acct.account_id).unwrap(), 0);
    assert!(
        s.push_tokens(&d.device_id).unwrap().is_empty(),
        "a revoked device must stop being wakeable"
    );
    // The row survives so `DeviceMeta.revoked` can be reported.
    let listed = s.list_devices(&acct.account_id).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].revoked);
    assert_eq!(listed[0].revoked_at_ms, Some(NOW + 1));
}

#[test]
fn one_account_cannot_revoke_or_read_anothers_device() {
    let s = store();
    let alice = s.resolve_account(&subject("alice"), true, NOW).unwrap();
    let bob = s.resolve_account(&subject("bob"), true, NOW).unwrap();
    let d = s
        .register_device(
            &alice.account_id,
            &NewDevice {
                device_pub_s: "k".into(),
                device_pub_d: None,
                device_cert: None,
                vault_device_id: None,
                nickname: "laptop".into(),
                platform: "linux".into(),
                app_version: None,
            },
            NOW,
        )
        .unwrap();
    assert!(matches!(
        s.revoke_device(&bob.account_id, &d.device_id, NOW),
        Err(StoreError::NotFound)
    ));
    assert!(s
        .active_device(&bob.account_id, &d.device_id)
        .unwrap()
        .is_none());
    assert!(s
        .active_device(&alice.account_id, &d.device_id)
        .unwrap()
        .is_some());
}

#[test]
fn revoking_twice_reports_not_found() {
    let s = store();
    let acct = s.resolve_account(&subject("alice"), true, NOW).unwrap();
    let d = s
        .register_device(
            &acct.account_id,
            &NewDevice {
                device_pub_s: "k".into(),
                device_pub_d: None,
                device_cert: None,
                vault_device_id: None,
                nickname: "phone".into(),
                platform: "ios".into(),
                app_version: None,
            },
            NOW,
        )
        .unwrap();
    s.revoke_device(&acct.account_id, &d.device_id, NOW)
        .unwrap();
    assert!(matches!(
        s.revoke_device(&acct.account_id, &d.device_id, NOW),
        Err(StoreError::NotFound)
    ));
}

/// A vault id is a *filter* on the caller's own rows, like every other
/// device id in this store, and this is the test that says so.
///
/// Two accounts can hold the same vault device id without contriving
/// anything — nothing coordinates 16-byte ids across accounts — so a
/// lookup by vault id alone, compared afterwards, would revoke a stranger's
/// device on a request that had every right to be made.
#[test]
fn one_account_cannot_revoke_anothers_device_by_its_vault_id() {
    const VAULT_ID: &str = "01J8ZQ7X9K3M5N7P9R1T3V5W7Y";
    let s = store();
    let alice = s.resolve_account(&subject("alice"), true, NOW).unwrap();
    let bob = s.resolve_account(&subject("bob"), true, NOW).unwrap();
    let d = s
        .register_device(
            &alice.account_id,
            &NewDevice {
                device_pub_s: "k".into(),
                device_pub_d: None,
                device_cert: None,
                vault_device_id: Some(VAULT_ID.into()),
                nickname: "laptop".into(),
                platform: "linux".into(),
                app_version: None,
            },
            NOW,
        )
        .unwrap();

    assert!(matches!(
        s.revoke_devices_by_vault_id(&bob.account_id, VAULT_ID, NOW),
        Err(StoreError::NotFound)
    ));
    assert!(
        s.active_device(&alice.account_id, &d.device_id)
            .unwrap()
            .is_some(),
        "alice's device must still be active"
    );

    assert_eq!(
        s.revoke_devices_by_vault_id(&alice.account_id, VAULT_ID, NOW)
            .unwrap(),
        1
    );
    assert!(s
        .active_device(&alice.account_id, &d.device_id)
        .unwrap()
        .is_none());
}

/// **What [`Store::open`] composes, and in what order.**
///
/// Migration 0001 runs `accounts::SCHEMA`, `devices::SCHEMA` and
/// `relay_log::SCHEMA`; migration 0002 adds `vault_device_id` where it is
/// missing and then the index over it. Those are names that can be
/// reordered or dropped independently, and only one of the orderings is
/// constrained by a failure:
/// `a_column_added_after_release_reaches_a_database_that_predates_it` below
/// fails if 0002 creates its index before its column.
///
/// The rest were free. Swapping `accounts::SCHEMA` and `devices::SCHEMA`
/// left the whole suite green, because SQLite resolves a `REFERENCES
/// accounts(account_id)` inside a `CREATE TABLE` lazily — the constraint is
/// enforced at DML time, not at declaration — so `open`'s own claim that
/// accounts come "first, because a device row references one" was a claim
/// nothing could falsify. Deleting the index statement left it green
/// too: no test named `devices_by_vault_id`, and an absent index changes a
/// query's plan rather than its answer.
///
/// Reading `sqlite_master` in `rowid` order is reading the DDL back in the
/// order it ran: rows are appended as objects are created, and a freshly
/// opened database has dropped nothing. Internal objects are filtered out —
/// `relay_frames`' `AUTOINCREMENT` mints a `sqlite_sequence`, and the
/// `PRIMARY KEY` / `UNIQUE` declarations mint `sqlite_autoindex_*` — because
/// those are SQLite's bookkeeping rather than this schema's composition.
#[test]
fn open_composes_each_tenants_ddl_in_dependency_order() {
    let s = store();
    let conn = s.conn.lock();

    let foreign_keys: i64 = conn
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        foreign_keys, 1,
        "every ON DELETE CASCADE in these tenants is inert without it"
    );

    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master
                 WHERE name NOT LIKE 'sqlite_%'
                 ORDER BY rowid",
        )
        .unwrap();
    let objects: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    assert_eq!(
        objects,
        vec![
            // accounts::SCHEMA — first, because the next table references it
            "accounts",
            // devices::SCHEMA
            "devices",
            "devices_by_account",
            "push_tokens",
            // relay_log::SCHEMA — references neither of the above
            "relay_frames",
            "relay_frames_by_channel",
            "relay_frame_heads",
            "relay_evicted",
            "relay_batches",
            "relay_batches_by_frame",
            // migration 0002 — after its column, which is what puts the
            // column this index names on an upgraded database
            "devices_by_vault_id",
            // migration 0003 — the deletion state, after the tables its
            // foreign keys name
            "accounts_pending_deletion",
            "account_delete_tokens",
            "blob_tombstones",
            "blob_tombstones_by_age",
            "device_cursors",
        ],
        "the DDL `open` composes, in the order it ran"
    );
    assert!(
        objects.contains(&"devices_by_vault_id".to_owned()),
        "the revoke-by-vault-id lookup's index, which no other test names"
    );
}

/// A column added to a table an earlier release created.
///
/// `SCHEMA` is `CREATE TABLE IF NOT EXISTS`, so it is inert against an
/// existing database: without migration 0002 a relay upgraded in place
/// would answer every query touching `vault_device_id` with "no such
/// column", which is every device query there is.
#[test]
fn a_column_added_after_release_reaches_a_database_that_predates_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sunrise.db");
    {
        // The `devices` table as it stood before this column existed.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE devices (
                     device_id       TEXT PRIMARY KEY,
                     account_id      TEXT NOT NULL,
                     device_pub_s    TEXT NOT NULL,
                     device_pub_d    TEXT,
                     device_cert     TEXT,
                     nickname        TEXT NOT NULL,
                     platform        TEXT NOT NULL,
                     app_version     TEXT,
                     created_at_ms   INTEGER NOT NULL,
                     last_seen_at_ms INTEGER NOT NULL,
                     revoked         INTEGER NOT NULL DEFAULT 0,
                     revoked_at_ms   INTEGER
                 );",
        )
        .unwrap();
    }

    let s = Store::open(Some(&path)).unwrap();
    let acct = s.resolve_account(&subject("alice"), true, NOW).unwrap();
    let d = s
        .register_device(
            &acct.account_id,
            &NewDevice {
                device_pub_s: "k".into(),
                device_pub_d: None,
                device_cert: None,
                vault_device_id: Some("01J8ZQ7X9K3M5N7P9R1T3V5W7Y".into()),
                nickname: "laptop".into(),
                platform: "linux".into(),
                app_version: None,
            },
            NOW,
        )
        .unwrap();
    assert_eq!(
        s.list_devices(&acct.account_id).unwrap()[0].vault_device_id,
        Some("01J8ZQ7X9K3M5N7P9R1T3V5W7Y".into())
    );
    assert!(s
        .active_device(&acct.account_id, &d.device_id)
        .unwrap()
        .is_some());
}

#[test]
fn identity_material_is_written_once_and_not_overwritten_by_a_retry() {
    let s = store();
    let acct = s.resolve_account(&subject("alice"), true, NOW).unwrap();
    let first = s
        .set_identity(&acct.account_id, "PUB_S", "PUB_D", Some("blob"), NOW)
        .unwrap();
    assert_eq!(first.identity_pub_s.as_deref(), Some("PUB_S"));
    let retry = s
        .set_identity(&acct.account_id, "ATTACKER", "ATTACKER", None, NOW + 1)
        .unwrap();
    assert_eq!(
        retry.identity_pub_s.as_deref(),
        Some("PUB_S"),
        "a second POST /accounts must not swap the identity key"
    );
    assert_eq!(retry.terms_at_ms, Some(NOW));
}

/// The recovery blob is write-once, and a *different* one is refused
/// rather than silently dropped.
///
/// Dropping it is what the `COALESCE` did, and it is a quiet way to
/// mislead a user: a second `sunrise bootstrap` seals a fresh blob, gets a
/// `201`, prints twenty-four words, and the account is still openable only
/// by the first code. Re-sending the identical bytes is still idempotent,
/// which is the retry case the route promises.
#[test]
fn a_second_different_recovery_blob_is_refused_and_the_same_one_is_not() {
    let s = store();
    let acct = s.resolve_account(&subject("alice"), true, NOW).unwrap();
    s.set_identity(&acct.account_id, "PUB_S", "PUB_D", Some("first"), NOW)
        .unwrap();

    assert!(matches!(
        s.set_identity(&acct.account_id, "PUB_S", "PUB_D", Some("second"), NOW + 1),
        Err(StoreError::RecoveryBlobExists)
    ));
    assert_eq!(
        s.recovery_blob(&acct.account_id).unwrap().as_deref(),
        Some("first"),
        "the refusal must not have written anything"
    );

    s.set_identity(&acct.account_id, "PUB_S", "PUB_D", Some("first"), NOW + 2)
        .expect("re-sending the same blob is the dropped-response retry");
}

/// The statement's own half of the rule, with the guard out of the way.
///
/// The test above never reaches the `UPDATE` with a conflicting blob — the
/// guard returns first — so it would still pass if `recovery_blob`'s
/// `COALESCE` were argument-first, which for a while it was. Driving
/// [`accounts::SET_IDENTITY`] directly is the only way to observe which of
/// its two operands wins, and it is what fails if the order is flipped
/// back.
///
/// Defence in depth, not the enforcement: the refusal the route answers
/// `409` with is the guard's, and a caller reaching this statement without
/// it gets silence rather than a conflict. What the assertion pins is that
/// the silence is a *no-op* and not an overwrite.
#[test]
fn the_update_statement_alone_never_moves_a_recovery_blob() {
    let s = store();
    let acct = s.resolve_account(&subject("alice"), true, NOW).unwrap();
    s.set_identity(&acct.account_id, "PUB_S", "PUB_D", Some("first"), NOW)
        .unwrap();

    let rows = {
        let conn = s.conn.lock();
        conn.execute(
            accounts::SET_IDENTITY,
            rusqlite::params![
                &acct.account_id,
                "PUB_S",
                "PUB_D",
                Some("second"),
                i64::try_from(NOW + 1).unwrap(),
            ],
        )
        .unwrap()
    };

    assert_eq!(rows, 1, "the statement must have matched the account row");
    assert_eq!(
        s.recovery_blob(&acct.account_id).unwrap().as_deref(),
        Some("first"),
        "the stored blob wins: the column is the first operand"
    );
}

/// An account with no blob reads as `None`, which is the state of every
/// account this relay holds today and is a `404` rather than a failure.
#[test]
fn an_account_without_a_recovery_blob_reads_as_absent() {
    let s = store();
    let acct = s.resolve_account(&subject("alice"), true, NOW).unwrap();
    assert_eq!(s.recovery_blob(&acct.account_id).unwrap(), None);
    assert_eq!(s.recovery_blob("nobody").unwrap(), None);

    s.set_identity(&acct.account_id, "PUB_S", "PUB_D", Some("blob"), NOW)
        .unwrap();
    assert_eq!(
        s.recovery_blob(&acct.account_id).unwrap().as_deref(),
        Some("blob")
    );
}

#[test]
fn a_file_backed_store_survives_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sunrise.db");
    let account_id = {
        let s = Store::open(Some(&path)).unwrap();
        s.resolve_account(&subject("alice"), true, NOW)
            .unwrap()
            .account_id
    };
    let s = Store::open(Some(&path)).unwrap();
    assert_eq!(
        s.resolve_account(&subject("alice"), false, NOW)
            .unwrap()
            .account_id,
        account_id,
        "a restart must not orphan existing accounts"
    );
}

#[test]
fn minted_ids_do_not_repeat_within_a_millisecond() {
    let a = mint_id(NOW);
    let b = mint_id(NOW);
    assert_ne!(a, b);
    assert_eq!(a.len(), sunrise_id::crockford::ENCODED_LEN);
}

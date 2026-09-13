//! The SQLite substrate the self-host server keeps its state in.
//!
//! This module owns the handle: one `Connection`, and a [`Store::open`] that
//! applies each tenant's tables to it in dependency order.
//!
//! **Accounts and devices** are the tenant implemented here, and each half
//! declares its own tables beside the statements that write them.
//! `docs/06-server/auth.md` keys an account on the OIDC pair `(iss, sub)` and
//! hangs devices off it, so `accounts` owns the account row and the identity
//! material written onto it, and `devices` owns the device rows
//! `docs/06-server/api.md` describes together with the push tokens that cascade
//! off them.
//!
//! **The durable relay log** is the other tenant, and it is entirely
//! [`crate::relay_log`]'s: its tables are declared there, beside the only
//! methods that read or write them. [`Store::open`] applies that DDL, because
//! one `Connection` opens one database — which is the only reason the relay is
//! named in this module at all.
//!
//! Backing file comes from [`crate::ServerConfig::sqlite_path`]; `None` opens
//! an in-memory database, which is what tests use — every `ServerState` then
//! owns a private database and no test can observe another's rows.
//!
//! # Concurrency
//!
//! One `Connection` behind a `parking_lot::Mutex`, held by both tenants. Every
//! statement *in this module* is a point lookup or a single-row write against a
//! small table, so the critical section is microseconds; a self-host relay
//! does not have the write volume to justify a pool, and a single connection
//! makes "revocation takes effect in the request's transaction" trivially
//! true. A multi-node deployment replaces this module with Postgres.
//!
//! The relay append path is the exception worth knowing about before reading
//! the paragraph above as the whole story: it takes this same mutex for a
//! multi-statement transaction and a retention sweep, so it holds the lock for
//! considerably longer than a point lookup does. [`crate::relay_log`] is where
//! that cost is described.

mod accounts;
mod devices;

use std::path::Path;

use parking_lot::Mutex;
use rusqlite::Connection;
use thiserror::Error;

// The inline suite below reaches this through its `use super::*`; every
// statement that names a subject itself is in `accounts`.
#[cfg(test)]
use crate::auth::Subject;

pub use accounts::Account;
pub use devices::{Device, NewDevice};

/// Why a store operation failed.
#[derive(Debug, Error)]
pub enum StoreError {
    /// Underlying SQLite failure.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The caller's `(iss, sub)` has no account and `allow_signup` is false.
    #[error("sign-up is disabled on this server")]
    SignupDisabled,
    /// No such row for this account.
    #[error("not found")]
    NotFound,
    /// A `recovery_blob` was offered for an account that already holds a
    /// different one.
    ///
    /// The column is write-once. It used to be silently so: `set_identity`
    /// wrote `COALESCE(?4, recovery_blob)`, so a second, different blob was
    /// accepted with a `201` and dropped on the floor — and the client that
    /// sent it had just shown its user a recovery code for a blob the server
    /// does not hold. Refusing is what makes a displayed code trustworthy;
    /// re-sending the *same* bytes is still idempotent and still succeeds.
    #[error("this account already holds a different recovery blob")]
    RecoveryBlobExists,
}

/// SQLite-backed account/device store.
pub struct Store {
    /// Shared with [`crate::relay_log`], which puts the durable relay op log
    /// in this same database so one file is the whole server's state and a
    /// `tar` of it is a consistent backup.
    pub(crate) conn: Mutex<Connection>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// Open the store. `None` opens a private in-memory database.
    ///
    /// Each tenant's DDL is applied in dependency order: accounts first,
    /// because a device row references one, then devices, then the relay log's
    /// own tables, which reference neither.
    pub fn open(path: Option<&Path>) -> Result<Self, StoreError> {
        let conn = match path {
            Some(p) => Connection::open(p)?,
            None => Connection::open_in_memory()?,
        };
        // The `ON DELETE CASCADE` each tenant declares is inert unless foreign
        // keys are on, and a revoked device leaving its push tokens behind
        // would keep waking a device its owner believes is gone.
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(accounts::SCHEMA)?;
        conn.execute_batch(devices::SCHEMA)?;
        conn.execute_batch(crate::relay_log::SCHEMA)?;
        devices::add_missing_columns(&conn)?;
        conn.execute_batch(devices::LATE_INDEXES)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

pub(super) fn unsigned(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// Mint a fresh 16-byte id, rendered Crockford base-32 per
/// `docs/06-server/api.md`.
///
/// ULID layout: the injected clock's millisecond stamp in the high 48 bits,
/// 80 bits of OS entropy below it. Timestamp-prefixed ids keep the SQLite
/// primary-key index appending rather than inserting into the middle, and the
/// entropy is what makes an id unguessable. `getrandom` is used directly rather
/// than a `rand` RNG because the determinism gate bans ambient `rand` sources
/// and there is nothing here worth seeding deterministically — an id a test
/// could predict would be an id an attacker could predict.
#[must_use]
pub(crate) fn mint_id(now_ms: u64) -> String {
    let mut random = [0u8; 10];
    if getrandom::getrandom(&mut random).is_err() {
        // No OS entropy is not a condition we can paper over with a
        // predictable id, so fall back to a value that is at least unique
        // per-process-per-ms and let the UNIQUE constraints catch collisions.
        random[..8].copy_from_slice(&now_ms.to_be_bytes());
    }
    let ulid = sunrise_id::ulid::Ulid::from_timestamp_and_random(now_ms, random);
    sunrise_id::crockford::encode_bytes(ulid.as_bytes())
}

#[cfg(test)]
mod tests {
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

    /// A column added to a table an earlier release created.
    ///
    /// `SCHEMA` is `CREATE TABLE IF NOT EXISTS`, so it is inert against an
    /// existing database: without `add_missing_columns` a relay upgraded in
    /// place would answer every query touching `vault_device_id` with "no such
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
}

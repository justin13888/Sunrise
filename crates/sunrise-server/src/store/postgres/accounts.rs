//! Accounts, devices and push tokens on Postgres: the statements
//! `store/accounts.rs` and `store/devices.rs` run on SQLite, each method one
//! transaction, with the same contracts.

use tokio_postgres::Row;

use super::{signed, unavailable, unsigned, PgStore};
use crate::auth::Subject;
use crate::store::tx::op;
use crate::store::{mint_id, Account, Device, MetadataError, NewDevice, Reinstatement};

const ACCOUNT_COLUMNS: &str = "account_id, oidc_iss, oidc_sub, email, identity_pub_s, \
     identity_pub_d, tier, terms_at_ms, created_at_ms";

const DEVICE_COLUMNS: &str = "device_id, account_id, device_pub_s, device_pub_d, \
     vault_device_id, nickname, platform, app_version, created_at_ms, last_seen_at_ms, revoked, \
     revoked_at_ms";

fn account(r: &Row) -> Account {
    Account {
        account_id: r.get(0),
        oidc_iss: r.get(1),
        oidc_sub: r.get(2),
        email: r.get(3),
        identity_pub_s: r.get(4),
        identity_pub_d: r.get(5),
        tier: r.get(6),
        terms_at_ms: r.get::<_, Option<i64>>(7).map(unsigned),
        created_at_ms: unsigned(r.get(8)),
    }
}

fn device(r: &Row) -> Device {
    Device {
        device_id: r.get(0),
        account_id: r.get(1),
        device_pub_s: r.get(2),
        device_pub_d: r.get(3),
        vault_device_id: r.get(4),
        nickname: r.get(5),
        platform: r.get(6),
        app_version: r.get(7),
        created_at_ms: unsigned(r.get(8)),
        last_seen_at_ms: unsigned(r.get(9)),
        revoked: r.get(10),
        revoked_at_ms: r.get::<_, Option<i64>>(11).map(unsigned),
    }
}

impl PgStore {
    /// `Store::resolve_account`. Two nodes provisioning one subject at once
    /// both end with the one row the unique `(iss, sub)` key lets in.
    pub(super) async fn resolve_account(
        &self,
        subject: &Subject,
        allow_signup: bool,
        now_ms: u64,
    ) -> Result<Account, MetadataError> {
        let _op = op("store.resolve_account");
        let c = self.client().await?;
        let by_oidc =
            format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE oidc_iss = $1 AND oidc_sub = $2");
        let found = c
            .query_opt(&by_oidc, &[&subject.issuer, &subject.subject])
            .await
            .map_err(unavailable)?;
        if let Some(row) = found {
            let mut a = account(&row);
            if let Some(email) = subject.email.as_deref() {
                if a.email.as_deref() != Some(email) {
                    c.execute(
                        "UPDATE accounts SET email = $1 WHERE account_id = $2",
                        &[&email, &a.account_id],
                    )
                    .await
                    .map_err(unavailable)?;
                    a.email = Some(email.to_owned());
                }
            }
            return Ok(a);
        }
        if !allow_signup {
            return Err(MetadataError::SignupDisabled);
        }
        c.execute(
            "INSERT INTO accounts (account_id, oidc_iss, oidc_sub, email, tier, created_at_ms)
             VALUES ($1, $2, $3, $4, 'free', $5)
             ON CONFLICT (oidc_iss, oidc_sub) DO NOTHING",
            &[
                &mint_id(now_ms),
                &subject.issuer,
                &subject.subject,
                &subject.email,
                &signed(now_ms),
            ],
        )
        .await
        .map_err(unavailable)?;
        let row = c
            .query_one(&by_oidc, &[&subject.issuer, &subject.subject])
            .await
            .map_err(unavailable)?;
        Ok(account(&row))
    }

    pub(super) async fn account_exists(&self, subject: &Subject) -> Result<bool, MetadataError> {
        let _op = op("store.account_exists");
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT 1 FROM accounts WHERE oidc_iss = $1 AND oidc_sub = $2",
                &[&subject.issuer, &subject.subject],
            )
            .await
            .map_err(unavailable)?;
        Ok(row.is_some())
    }

    pub(super) async fn account(&self, account_id: &str) -> Result<Option<Account>, MetadataError> {
        let _op = op("store.account");
        let sql = format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE account_id = $1");
        let row = self
            .client()
            .await?
            .query_opt(&sql, &[&account_id])
            .await
            .map_err(unavailable)?;
        Ok(row.as_ref().map(account))
    }

    /// `Store::set_identity`: the write-once guard and the write, under the
    /// account row's lock, so two nodes cannot both pass the guard.
    pub(super) async fn set_identity(
        &self,
        account_id: &str,
        identity_pub_s: &str,
        identity_pub_d: &str,
        recovery_blob: Option<&str>,
        terms_at_ms: u64,
    ) -> Result<Account, MetadataError> {
        let _op = op("store.set_identity");
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(unavailable)?;
        let existing = tx
            .query_opt(
                "SELECT recovery_blob FROM accounts WHERE account_id = $1 FOR UPDATE",
                &[&account_id],
            )
            .await
            .map_err(unavailable)?
            .ok_or(MetadataError::NotFound)?;
        if let (Some(offered), Some(held)) = (recovery_blob, existing.get::<_, Option<&str>>(0)) {
            if held != offered {
                return Err(MetadataError::RecoveryBlobExists);
            }
        }
        let sql = format!(
            "UPDATE accounts
                SET identity_pub_s = COALESCE(identity_pub_s, $2),
                    identity_pub_d = COALESCE(identity_pub_d, $3),
                    recovery_blob  = COALESCE(recovery_blob, $4),
                    terms_at_ms    = COALESCE(terms_at_ms, $5)
              WHERE account_id = $1
          RETURNING {ACCOUNT_COLUMNS}"
        );
        let row = tx
            .query_one(
                &sql,
                &[
                    &account_id,
                    &identity_pub_s,
                    &identity_pub_d,
                    &recovery_blob,
                    &signed(terms_at_ms),
                ],
            )
            .await
            .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        Ok(account(&row))
    }

    pub(super) async fn recovery_blob(
        &self,
        account_id: &str,
    ) -> Result<Option<String>, MetadataError> {
        let _op = op("store.recovery_blob");
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT recovery_blob FROM accounts WHERE account_id = $1",
                &[&account_id],
            )
            .await
            .map_err(unavailable)?;
        Ok(row.and_then(|r| r.get(0)))
    }

    pub(super) async fn register_device(
        &self,
        account_id: &str,
        new: &NewDevice,
        now_ms: u64,
    ) -> Result<Device, MetadataError> {
        let _op = op("store.register_device");
        let device_id = mint_id(now_ms);
        let now = signed(now_ms);
        self.client()
            .await?
            .execute(
                "INSERT INTO devices (device_id, account_id, device_pub_s, device_pub_d,
                     device_cert, vault_device_id, nickname, platform, app_version,
                     created_at_ms, last_seen_at_ms, revoked)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $10, FALSE)",
                &[
                    &device_id,
                    &account_id,
                    &new.device_pub_s,
                    &new.device_pub_d,
                    &new.device_cert,
                    &new.vault_device_id,
                    &new.nickname,
                    &new.platform,
                    &new.app_version,
                    &now,
                ],
            )
            .await
            .map_err(unavailable)?;
        Ok(Device {
            device_id,
            account_id: account_id.to_owned(),
            device_pub_s: new.device_pub_s.clone(),
            device_pub_d: new.device_pub_d.clone(),
            vault_device_id: new.vault_device_id.clone(),
            nickname: new.nickname.clone(),
            platform: new.platform.clone(),
            app_version: new.app_version.clone(),
            created_at_ms: now_ms,
            last_seen_at_ms: now_ms,
            revoked: false,
            revoked_at_ms: None,
        })
    }

    pub(super) async fn list_devices(
        &self,
        account_id: &str,
    ) -> Result<Vec<Device>, MetadataError> {
        let _op = op("store.list_devices");
        let sql = format!(
            "SELECT {DEVICE_COLUMNS} FROM devices WHERE account_id = $1
             ORDER BY created_at_ms, device_id"
        );
        let rows = self
            .client()
            .await?
            .query(&sql, &[&account_id])
            .await
            .map_err(unavailable)?;
        Ok(rows.iter().map(device).collect())
    }

    pub(super) async fn active_device_count(&self, account_id: &str) -> Result<u32, MetadataError> {
        let _op = op("store.active_device_count");
        let n: i64 = self
            .client()
            .await?
            .query_one(
                "SELECT COUNT(*) FROM devices WHERE account_id = $1 AND NOT revoked",
                &[&account_id],
            )
            .await
            .map_err(unavailable)?
            .get(0);
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    pub(super) async fn active_device(
        &self,
        account_id: &str,
        device_id: &str,
    ) -> Result<Option<Device>, MetadataError> {
        let _op = op("store.active_device");
        let sql = format!(
            "SELECT {DEVICE_COLUMNS} FROM devices
             WHERE account_id = $1 AND device_id = $2 AND NOT revoked"
        );
        let row = self
            .client()
            .await?
            .query_opt(&sql, &[&account_id, &device_id])
            .await
            .map_err(unavailable)?;
        Ok(row.as_ref().map(device))
    }

    /// `Store::revoke_device`: the flag and the push tokens in one
    /// transaction.
    pub(super) async fn revoke_device(
        &self,
        account_id: &str,
        device_id: &str,
        now_ms: u64,
    ) -> Result<(), MetadataError> {
        let _op = op("store.revoke_device");
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(unavailable)?;
        let changed = tx
            .execute(
                "UPDATE devices SET revoked = TRUE, revoked_at_ms = $3
                 WHERE account_id = $1 AND device_id = $2 AND NOT revoked",
                &[&account_id, &device_id, &signed(now_ms)],
            )
            .await
            .map_err(unavailable)?;
        if changed == 0 {
            return Err(MetadataError::NotFound);
        }
        tx.execute(
            "DELETE FROM push_tokens WHERE device_id = $1",
            &[&device_id],
        )
        .await
        .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)
    }

    pub(super) async fn revoke_devices_by_vault_id(
        &self,
        account_id: &str,
        vault_device_id: &str,
        revoked_by_key: Option<&str>,
        now_ms: u64,
    ) -> Result<usize, MetadataError> {
        let _op = op("store.revoke_devices_by_vault_id");
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(unavailable)?;
        let ids: Vec<String> = tx
            .query(
                "UPDATE devices SET revoked = TRUE, revoked_at_ms = $3, revoked_by_key = $4
                 WHERE account_id = $1 AND vault_device_id = $2 AND NOT revoked
                 RETURNING device_id",
                &[
                    &account_id,
                    &vault_device_id,
                    &signed(now_ms),
                    &revoked_by_key,
                ],
            )
            .await
            .map_err(unavailable)?
            .iter()
            .map(|r| r.get(0))
            .collect();
        if ids.is_empty() {
            return Err(MetadataError::NotFound);
        }
        tx.execute("DELETE FROM push_tokens WHERE device_id = ANY($1)", &[&ids])
            .await
            .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        Ok(ids.len())
    }

    /// `Store::reinstate_devices_by_vault_id`: the caller's own revocations of
    /// that vault id undone, and what the rows say when there were none.
    pub(super) async fn reinstate_devices_by_vault_id(
        &self,
        account_id: &str,
        vault_device_id: &str,
        caller_key: &str,
    ) -> Result<Reinstatement, MetadataError> {
        let _op = op("store.reinstate_devices_by_vault_id");
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(unavailable)?;
        let changed = tx
            .execute(
                "UPDATE devices SET revoked = FALSE, revoked_at_ms = NULL, revoked_by_key = NULL
                 WHERE account_id = $1 AND vault_device_id = $2 AND revoked AND revoked_by_key = $3",
                &[&account_id, &vault_device_id, &caller_key],
            )
            .await
            .map_err(unavailable)?;
        let outcome = if changed > 0 {
            Reinstatement::Reinstated(usize::try_from(changed).unwrap_or(usize::MAX))
        } else {
            let row = tx
                .query_one(
                    "SELECT COUNT(*) FILTER (WHERE NOT revoked), COUNT(*) FILTER (WHERE revoked)
                     FROM devices WHERE account_id = $1 AND vault_device_id = $2",
                    &[&account_id, &vault_device_id],
                )
                .await
                .map_err(unavailable)?;
            let (active, revoked): (i64, i64) = (row.get(0), row.get(1));
            if active > 0 {
                Reinstatement::AlreadyActive
            } else if revoked > 0 {
                Reinstatement::NotRevoker
            } else {
                return Err(MetadataError::NotFound);
            }
        };
        tx.commit().await.map_err(unavailable)?;
        Ok(outcome)
    }

    pub(super) async fn touch_device(
        &self,
        device_id: &str,
        now_ms: u64,
    ) -> Result<(), MetadataError> {
        let _op = op("store.touch_device");
        self.client()
            .await?
            .execute(
                "UPDATE devices SET last_seen_at_ms = $2 WHERE device_id = $1",
                &[&device_id, &signed(now_ms)],
            )
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    pub(super) async fn upsert_push_token(
        &self,
        device_id: &str,
        platform: &str,
        token: &str,
        now_ms: u64,
    ) -> Result<(), MetadataError> {
        let _op = op("store.upsert_push_token");
        self.client()
            .await?
            .execute(
                "INSERT INTO push_tokens (device_id, platform, token, updated_at_ms)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (device_id, platform) DO UPDATE
                    SET token = EXCLUDED.token, updated_at_ms = EXCLUDED.updated_at_ms",
                &[&device_id, &platform, &token, &signed(now_ms)],
            )
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    pub(super) async fn push_tokens(
        &self,
        device_id: &str,
    ) -> Result<Vec<(String, String)>, MetadataError> {
        let _op = op("store.push_tokens");
        let rows = self
            .client()
            .await?
            .query(
                "SELECT platform, token FROM push_tokens WHERE device_id = $1 ORDER BY platform",
                &[&device_id],
            )
            .await
            .map_err(unavailable)?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    pub(super) async fn push_targets(
        &self,
        account_id: &str,
        platform: &str,
    ) -> Result<Vec<(String, String)>, MetadataError> {
        let _op = op("store.push_targets");
        let rows = self
            .client()
            .await?
            .query(
                "SELECT d.device_id, p.token FROM devices d
                 JOIN push_tokens p ON p.device_id = d.device_id
                 WHERE d.account_id = $1 AND NOT d.revoked AND p.platform = $2
                 ORDER BY d.device_id",
                &[&account_id, &platform],
            )
            .await
            .map_err(unavailable)?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    pub(super) async fn push_target(
        &self,
        device_id: &str,
        platform: &str,
    ) -> Result<Option<String>, MetadataError> {
        let _op = op("store.push_target");
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT p.token FROM push_tokens p
                 JOIN devices d ON d.device_id = p.device_id
                 WHERE p.device_id = $1 AND p.platform = $2 AND NOT d.revoked",
                &[&device_id, &platform],
            )
            .await
            .map_err(unavailable)?;
        Ok(row.map(|r| r.get(0)))
    }

    pub(super) async fn delete_push_token(
        &self,
        device_id: &str,
        platform: &str,
        token: &str,
    ) -> Result<bool, MetadataError> {
        let _op = op("store.delete_push_token");
        let n = self
            .client()
            .await?
            .execute(
                "DELETE FROM push_tokens WHERE device_id = $1 AND platform = $2 AND token = $3",
                &[&device_id, &platform, &token],
            )
            .await
            .map_err(unavailable)?;
        Ok(n > 0)
    }

    pub(super) async fn device_owner(
        &self,
        device_id: &str,
    ) -> Result<Option<String>, MetadataError> {
        let _op = op("store.device_owner");
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT account_id FROM devices WHERE device_id = $1",
                &[&device_id],
            )
            .await
            .map_err(unavailable)?;
        Ok(row.map(|r| r.get(0)))
    }
}

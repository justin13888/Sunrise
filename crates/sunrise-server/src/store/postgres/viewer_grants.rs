//! `store/viewer_grants.rs` over Postgres: the relay viewer grants of
//! ADR-0074, with the same answers as the SQLite store.
//!
//! A registration locks the owner's account row, so two nodes registering for
//! one owner cannot both pass the cap; a claim locks the grant's row, so two
//! accounts claiming one token cannot both be bound.

use tokio_postgres::Row;

use super::lifecycle::lock_account;
use super::{key16, signed, unavailable, unsigned, PgStore};
use crate::store::tx::op;
use crate::store::{GrantCreated, MetadataError, ViewerGrant, MAX_GRANTS_PER_OWNER};

const COLUMNS: &str = "grant_id, owner_id, stream_id, recipient_id, expires_at_ms, \
                       created_ms, claimed_ms";

fn grant(r: &Row) -> Option<ViewerGrant> {
    Some(ViewerGrant {
        grant_id: key16(r.get(0))?,
        owner_id: r.get(1),
        stream_id: key16(r.get(2))?,
        recipient_id: r.get(3),
        expires_at_ms: r.get::<_, Option<i64>>(4).map(unsigned),
        created_ms: unsigned(r.get(5)),
        claimed_ms: r.get::<_, Option<i64>>(6).map(unsigned),
    })
}

impl PgStore {
    /// `Store::create_viewer_grant`.
    pub(super) async fn create_viewer_grant(
        &self,
        g: &ViewerGrant,
        claim_h: &[u8; 32],
        now_ms: u64,
    ) -> Result<GrantCreated, MetadataError> {
        let _op = op("store.create_viewer_grant");
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(unavailable)?;
        lock_account(&tx, &g.owner_id).await?;
        let existing = tx
            .query_opt(
                &format!("SELECT {COLUMNS} FROM viewer_grants WHERE claim_h = $1"),
                &[&&claim_h[..]],
            )
            .await
            .map_err(unavailable)?;
        if let Some(existing) = existing.as_ref().and_then(grant) {
            let retried = existing.owner_id == g.owner_id
                && existing.stream_id == g.stream_id
                && existing.expires_at_ms == g.expires_at_ms;
            return Ok(if retried {
                GrantCreated::Exists(existing)
            } else {
                GrantCreated::TokenTaken
            });
        }
        tx.execute(
            "DELETE FROM viewer_grants
             WHERE owner_id = $1 AND expires_at_ms IS NOT NULL AND expires_at_ms <= $2",
            &[&g.owner_id, &signed(now_ms)],
        )
        .await
        .map_err(unavailable)?;
        let held: i64 = tx
            .query_one(
                "SELECT COUNT(*) FROM viewer_grants WHERE owner_id = $1",
                &[&g.owner_id],
            )
            .await
            .map_err(unavailable)?
            .get(0);
        if unsigned(held) >= MAX_GRANTS_PER_OWNER {
            return Ok(GrantCreated::TooMany);
        }
        tx.execute(
            "INSERT INTO viewer_grants
                 (grant_id, owner_id, stream_id, claim_h, expires_at_ms, created_ms)
             VALUES ($1, $2, $3, $4, $5, $6)",
            &[
                &&g.grant_id[..],
                &g.owner_id,
                &&g.stream_id[..],
                &&claim_h[..],
                &g.expires_at_ms.map(signed),
                &signed(g.created_ms),
            ],
        )
        .await
        .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        Ok(GrantCreated::Created(ViewerGrant {
            recipient_id: None,
            claimed_ms: None,
            ..g.clone()
        }))
    }

    /// `Store::claim_viewer_grant`.
    pub(super) async fn claim_viewer_grant(
        &self,
        claim_h: &[u8; 32],
        account_id: &str,
        now_ms: u64,
    ) -> Result<Option<ViewerGrant>, MetadataError> {
        let _op = op("store.claim_viewer_grant");
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(unavailable)?;
        let found = tx
            .query_opt(
                &format!("SELECT {COLUMNS} FROM viewer_grants WHERE claim_h = $1 FOR UPDATE"),
                &[&&claim_h[..]],
            )
            .await
            .map_err(unavailable)?;
        let Some(found) = found.as_ref().and_then(grant) else {
            return Ok(None);
        };
        if found.expired(now_ms) || found.owner_id == account_id {
            return Ok(None);
        }
        match found.recipient_id.as_deref() {
            Some(holder) if holder == account_id => return Ok(Some(found)),
            Some(_) => return Ok(None),
            None => {}
        }
        tx.execute(
            "UPDATE viewer_grants SET recipient_id = $1, claimed_ms = $2
             WHERE grant_id = $3 AND recipient_id IS NULL",
            &[&account_id, &signed(now_ms), &&found.grant_id[..]],
        )
        .await
        .map_err(unavailable)?;
        tx.commit().await.map_err(unavailable)?;
        Ok(Some(ViewerGrant {
            recipient_id: Some(account_id.to_owned()),
            claimed_ms: Some(now_ms),
            ..found
        }))
    }

    /// `Store::viewer_grant`.
    pub(super) async fn viewer_grant(
        &self,
        grant_id: &[u8; 16],
    ) -> Result<Option<ViewerGrant>, MetadataError> {
        let _op = op("store.viewer_grant");
        let row = self
            .client()
            .await?
            .query_opt(
                &format!("SELECT {COLUMNS} FROM viewer_grants WHERE grant_id = $1"),
                &[&&grant_id[..]],
            )
            .await
            .map_err(unavailable)?;
        Ok(row.as_ref().and_then(grant))
    }

    /// `Store::viewer_grants`.
    pub(super) async fn viewer_grants(
        &self,
        account_id: &str,
    ) -> Result<Vec<ViewerGrant>, MetadataError> {
        let _op = op("store.viewer_grants");
        let rows = self
            .client()
            .await?
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM viewer_grants
                     WHERE owner_id = $1 OR recipient_id = $1
                     ORDER BY created_ms, grant_id"
                ),
                &[&account_id],
            )
            .await
            .map_err(unavailable)?;
        Ok(rows.iter().filter_map(grant).collect())
    }

    /// `Store::withdraw_viewer_grant`.
    pub(super) async fn withdraw_viewer_grant(
        &self,
        grant_id: &[u8; 16],
        account_id: &str,
    ) -> Result<bool, MetadataError> {
        let _op = op("store.withdraw_viewer_grant");
        let n = self
            .client()
            .await?
            .execute(
                "DELETE FROM viewer_grants
                 WHERE grant_id = $1 AND (owner_id = $2 OR recipient_id = $2)",
                &[&&grant_id[..], &account_id],
            )
            .await
            .map_err(unavailable)?;
        Ok(n > 0)
    }
}

//! Relay viewer grants: which account may read which other account's Stream
//! (ADR-0074).
//!
//! A grant is registered by the Stream's owner with the hash of a claim token,
//! and bound to a recipient account by the first other account to present the
//! token. A session subscribing through a claimed grant reads the owner's
//! channel. The grant decides only whom the relay hands ciphertext to: the
//! Stream key reaches the recipient sealed to its identity, and the relay
//! never holds it.

use rusqlite::{params, OptionalExtension, Row};

use super::{unsigned, Store, StoreError};

/// Migration 0010, and therefore frozen: change this table by a new
/// migration, never an edit here.
///
/// Cascades from both account rows, so erasing either party erases the grant.
/// `claim_h` is the domain-separated BLAKE3 of the claim token; the token
/// itself is never stored. `recipient_id` and `claimed_ms` are `NULL` until a
/// claim binds them.
pub(super) const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS viewer_grants (
    grant_id      BLOB PRIMARY KEY,
    owner_id      TEXT NOT NULL REFERENCES accounts(account_id) ON DELETE CASCADE,
    stream_id     BLOB NOT NULL,
    claim_h       BLOB NOT NULL UNIQUE,
    recipient_id  TEXT REFERENCES accounts(account_id) ON DELETE CASCADE,
    expires_at_ms INTEGER,
    created_ms    INTEGER NOT NULL,
    claimed_ms    INTEGER
);
CREATE INDEX IF NOT EXISTS viewer_grants_by_owner ON viewer_grants(owner_id);
CREATE INDEX IF NOT EXISTS viewer_grants_by_recipient ON viewer_grants(recipient_id);";

/// The most grants one owner may hold that have not expired (ADR-0074 §5).
pub const MAX_GRANTS_PER_OWNER: u64 = 256;

/// One relay viewer grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerGrant {
    /// The relay's id for the grant, drawn at random when it is registered.
    pub grant_id: [u8; 16],
    /// The account whose Stream it is.
    pub owner_id: String,
    /// The owner's Stream the grant reads.
    pub stream_id: [u8; 16],
    /// The account the grant is bound to, once claimed.
    pub recipient_id: Option<String>,
    /// When the grant stops being served, ms since the epoch; `None` for
    /// never.
    pub expires_at_ms: Option<u64>,
    /// When the owner registered it, by the relay's clock.
    pub created_ms: u64,
    /// When the recipient claimed it, by the relay's clock.
    pub claimed_ms: Option<u64>,
}

impl ViewerGrant {
    /// Whether the grant has expired at `now_ms`: at or after `expires_at_ms`,
    /// the boundary the recipient's own check uses.
    #[must_use]
    pub fn expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms.is_some_and(|at| now_ms >= at)
    }

    /// Whether `account_id` may read the owner's Stream through this grant at
    /// `now_ms`: the grant is claimed by that account and has not expired.
    #[must_use]
    pub fn serves(&self, account_id: &str, now_ms: u64) -> bool {
        self.recipient_id.as_deref() == Some(account_id) && !self.expired(now_ms)
    }
}

/// What [`Store::create_viewer_grant`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantCreated {
    /// The grant is registered.
    Created(ViewerGrant),
    /// The owner had already registered this claim token for this Stream:
    /// a retried registration, answered with the grant it made.
    Exists(ViewerGrant),
    /// The claim token is registered to another grant. Nothing was written.
    TokenTaken,
    /// The owner already holds [`MAX_GRANTS_PER_OWNER`] grants that have not
    /// expired. Nothing was written.
    TooMany,
}

const COLUMNS: &str = "grant_id, owner_id, stream_id, recipient_id, expires_at_ms, \
                       created_ms, claimed_ms";

fn signed(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn grant(r: &Row<'_>) -> rusqlite::Result<Option<ViewerGrant>> {
    let grant_id: Vec<u8> = r.get(0)?;
    let stream_id: Vec<u8> = r.get(2)?;
    let (Ok(grant_id), Ok(stream_id)) = (grant_id.try_into(), stream_id.try_into()) else {
        return Ok(None);
    };
    Ok(Some(ViewerGrant {
        grant_id,
        owner_id: r.get(1)?,
        stream_id,
        recipient_id: r.get(3)?,
        expires_at_ms: r.get::<_, Option<i64>>(4)?.map(unsigned),
        created_ms: unsigned(r.get(5)?),
        claimed_ms: r.get::<_, Option<i64>>(6)?.map(unsigned),
    }))
}

fn by_claim(
    tx: &rusqlite::Transaction<'_>,
    claim_h: &[u8; 32],
) -> rusqlite::Result<Option<ViewerGrant>> {
    Ok(tx
        .query_row(
            &format!("SELECT {COLUMNS} FROM viewer_grants WHERE claim_h = ?1"),
            params![&claim_h[..]],
            grant,
        )
        .optional()?
        .flatten())
}

impl Store {
    /// Register `grant` under `claim_h`, unclaimed.
    ///
    /// The owner's expired grants are deleted first, in the same transaction,
    /// so the cap counts only the grants still in force or awaiting a claim.
    pub fn create_viewer_grant(
        &self,
        grant: &ViewerGrant,
        claim_h: &[u8; 32],
        now_ms: u64,
    ) -> Result<GrantCreated, StoreError> {
        let mut conn = self.tx("store.create_viewer_grant");
        let tx = conn.transaction()?;
        if let Some(existing) = by_claim(&tx, claim_h)? {
            let retried = existing.owner_id == grant.owner_id
                && existing.stream_id == grant.stream_id
                && existing.expires_at_ms == grant.expires_at_ms;
            return Ok(if retried {
                GrantCreated::Exists(existing)
            } else {
                GrantCreated::TokenTaken
            });
        }
        tx.execute(
            "DELETE FROM viewer_grants
             WHERE owner_id = ?1 AND expires_at_ms IS NOT NULL AND expires_at_ms <= ?2",
            params![grant.owner_id, signed(now_ms)],
        )?;
        let held: i64 = tx.query_row(
            "SELECT COUNT(*) FROM viewer_grants WHERE owner_id = ?1",
            params![grant.owner_id],
            |r| r.get(0),
        )?;
        if unsigned(held) >= MAX_GRANTS_PER_OWNER {
            return Ok(GrantCreated::TooMany);
        }
        tx.execute(
            "INSERT INTO viewer_grants
                 (grant_id, owner_id, stream_id, claim_h, expires_at_ms, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                &grant.grant_id[..],
                grant.owner_id,
                &grant.stream_id[..],
                &claim_h[..],
                grant.expires_at_ms.map(signed),
                signed(grant.created_ms),
            ],
        )?;
        tx.commit()?;
        Ok(GrantCreated::Created(ViewerGrant {
            recipient_id: None,
            claimed_ms: None,
            ..grant.clone()
        }))
    }

    /// Bind the grant registered under `claim_h` to `account_id`, if it is
    /// unclaimed, unexpired, and not the account's own; or answer the grant
    /// again if that account already holds it.
    ///
    /// `None` for every other case, alike: no grant under the token, an
    /// expired one, the owner's own, or one another account holds.
    pub fn claim_viewer_grant(
        &self,
        claim_h: &[u8; 32],
        account_id: &str,
        now_ms: u64,
    ) -> Result<Option<ViewerGrant>, StoreError> {
        let mut conn = self.tx("store.claim_viewer_grant");
        let tx = conn.transaction()?;
        let Some(found) = by_claim(&tx, claim_h)? else {
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
            "UPDATE viewer_grants SET recipient_id = ?1, claimed_ms = ?2
             WHERE grant_id = ?3 AND recipient_id IS NULL",
            params![account_id, signed(now_ms), &found.grant_id[..]],
        )?;
        tx.commit()?;
        Ok(Some(ViewerGrant {
            recipient_id: Some(account_id.to_owned()),
            claimed_ms: Some(now_ms),
            ..found
        }))
    }

    /// The grant under `grant_id`, whoever holds it.
    pub fn viewer_grant(&self, grant_id: &[u8; 16]) -> Result<Option<ViewerGrant>, StoreError> {
        Ok(self
            .tx("store.viewer_grant")
            .query_row(
                &format!("SELECT {COLUMNS} FROM viewer_grants WHERE grant_id = ?1"),
                params![&grant_id[..]],
                grant,
            )
            .optional()?
            .flatten())
    }

    /// Every grant `account_id` owns or holds, oldest first.
    pub fn viewer_grants(&self, account_id: &str) -> Result<Vec<ViewerGrant>, StoreError> {
        let conn = self.tx("store.viewer_grants");
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM viewer_grants
             WHERE owner_id = ?1 OR recipient_id = ?1
             ORDER BY created_ms, grant_id"
        ))?;
        let rows = stmt.query_map(params![account_id], grant)?;
        let mut out = Vec::new();
        for row in rows {
            out.extend(row?);
        }
        Ok(out)
    }

    /// Delete the grant under `grant_id` if `account_id` owns or holds it.
    /// `false` when there was none it was party to.
    pub fn withdraw_viewer_grant(
        &self,
        grant_id: &[u8; 16],
        account_id: &str,
    ) -> Result<bool, StoreError> {
        let n = self.tx("store.withdraw_viewer_grant").execute(
            "DELETE FROM viewer_grants
             WHERE grant_id = ?1 AND (owner_id = ?2 OR recipient_id = ?2)",
            params![&grant_id[..], account_id],
        )?;
        Ok(n > 0)
    }
}

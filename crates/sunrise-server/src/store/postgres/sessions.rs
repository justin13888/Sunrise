//! Sync sessions in one shared table (ADR-0062 §3), so a session opened on
//! one relay node is honoured on every other.
//!
//! The row is keyed by `BLAKE3(session_id)`, never the id: an id is
//! bearer-equivalent, and a read of the table or a backup of it must not
//! yield one anyone could present. The session itself is the row's `body`, as
//! JSON, beside the two columns collection reads, `deadline_ms` and `seen_ms`.
//! An update reads the row `FOR UPDATE`, so two nodes applying a subscribe and
//! a refresh to one session take turns rather than lose one.

use super::{signed, PgStore};
use crate::sync_session::{Session, SessionBackend, SessionEdit, SessionError, IDLE_TTL_MS};

/// The Postgres [`SessionBackend`], on the same pool as the metadata.
#[derive(Debug, Clone)]
pub struct PgSessions {
    store: PgStore,
}

impl PgSessions {
    /// Sessions in `store`'s database.
    #[must_use]
    pub fn new(store: PgStore) -> Self {
        Self { store }
    }

    async fn client(&self) -> Result<deadpool_postgres::Object, SessionError> {
        self.store.client().await.map_err(down)
    }
}

fn down(e: impl std::fmt::Display) -> SessionError {
    SessionError::Unavailable(e.to_string())
}

/// The key a session id is filed under.
fn hashed(id: &str) -> Vec<u8> {
    blake3::hash(id.as_bytes()).as_bytes().to_vec()
}

fn encode(session: &Session) -> Result<String, SessionError> {
    serde_json::to_string(session).map_err(down)
}

fn decode(body: &str) -> Result<Session, SessionError> {
    serde_json::from_str(body).map_err(down)
}

fn deadline(session: &Session) -> Option<i64> {
    session.deadline_ms.map(signed)
}

#[async_trait::async_trait]
impl SessionBackend for PgSessions {
    async fn insert(&self, id: String, session: Session) -> Result<(), SessionError> {
        self.client()
            .await?
            .execute(
                "INSERT INTO sync_sessions (session_h, deadline_ms, seen_ms, body)
                 VALUES ($1, $2, $3, $4)",
                &[
                    &hashed(&id),
                    &deadline(&session),
                    &signed(session.seen_ms),
                    &encode(&session)?,
                ],
            )
            .await
            .map_err(down)?;
        Ok(())
    }

    async fn get(&self, id: &str, now_ms: u64) -> Result<Option<Session>, SessionError> {
        let c = self.client().await?;
        let key = hashed(id);
        let Some(row) = c
            .query_opt(
                "SELECT body FROM sync_sessions WHERE session_h = $1",
                &[&key],
            )
            .await
            .map_err(down)?
        else {
            return Ok(None);
        };
        let session = decode(row.get(0))?;
        if !session.expired(now_ms) {
            return Ok(Some(session));
        }
        // Deleted only if it is still expired: another node may have refreshed
        // it since the read, and its new deadline must stand.
        let deleted = c
            .execute(
                "DELETE FROM sync_sessions
                 WHERE session_h = $1 AND deadline_ms IS NOT NULL AND deadline_ms <= $2",
                &[&key, &signed(now_ms)],
            )
            .await
            .map_err(down)?;
        if deleted == 1 {
            return Ok(None);
        }
        // Refreshed (or removed) in between: what the row holds now decides.
        let Some(row) = c
            .query_opt(
                "SELECT body FROM sync_sessions WHERE session_h = $1",
                &[&key],
            )
            .await
            .map_err(down)?
        else {
            return Ok(None);
        };
        let session = decode(row.get(0))?;
        Ok((!session.expired(now_ms)).then_some(session))
    }

    async fn update(&self, id: &str, now_ms: u64, edit: SessionEdit) -> Result<bool, SessionError> {
        let mut c = self.client().await?;
        let key = hashed(id);
        let tx = c.transaction().await.map_err(down)?;
        let Some(row) = tx
            .query_opt(
                "SELECT body FROM sync_sessions WHERE session_h = $1 FOR UPDATE",
                &[&key],
            )
            .await
            .map_err(down)?
        else {
            return Ok(false);
        };
        let mut session = decode(row.get(0))?;
        if session.expired(now_ms) {
            tx.execute("DELETE FROM sync_sessions WHERE session_h = $1", &[&key])
                .await
                .map_err(down)?;
            tx.commit().await.map_err(down)?;
            return Ok(false);
        }
        session.seen_ms = now_ms;
        edit(&mut session);
        tx.execute(
            "UPDATE sync_sessions SET deadline_ms = $2, seen_ms = $3, body = $4
             WHERE session_h = $1",
            &[
                &key,
                &deadline(&session),
                &signed(session.seen_ms),
                &encode(&session)?,
            ],
        )
        .await
        .map_err(down)?;
        tx.commit().await.map_err(down)?;
        Ok(true)
    }

    async fn remove(&self, id: &str) -> Result<(), SessionError> {
        self.client()
            .await?
            .execute(
                "DELETE FROM sync_sessions WHERE session_h = $1",
                &[&hashed(id)],
            )
            .await
            .map_err(down)?;
        Ok(())
    }

    async fn collect(&self, now_ms: u64) -> Result<(), SessionError> {
        let now = signed(now_ms);
        let idle_before = now.saturating_sub(signed(IDLE_TTL_MS));
        self.client()
            .await?
            .execute(
                "DELETE FROM sync_sessions
                 WHERE (deadline_ms IS NOT NULL AND deadline_ms <= $1) OR seen_ms <= $2",
                &[&now, &idle_before],
            )
            .await
            .map_err(down)?;
        Ok(())
    }

    async fn count(&self) -> Result<usize, SessionError> {
        let n: i64 = self
            .client()
            .await?
            .query_one("SELECT COUNT(*) FROM sync_sessions", &[])
            .await
            .map_err(down)?
            .get(0);
        Ok(usize::try_from(n).unwrap_or(0))
    }
}

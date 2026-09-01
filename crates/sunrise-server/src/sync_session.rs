//! The negotiated state a sync session carries between its operations.
//!
//! On the WebSocket this state was the connection: `Hello` established it once,
//! the socket held it, and closing the socket discarded it. ADR-0023 replaces
//! that connection with four independent HTTP operations, so the state needs
//! somewhere to live and an id to be named by — which is what
//! `POST /sync/session` returns.
//!
//! # Why the id travels in a header
//!
//! `X-Sunrise-Session`, not `?session=`. A session id is a bearer-equivalent:
//! anything holding one can read the account's op stream. The query string is
//! where this server already leaked a credential once — browser clients put
//! `?access_token=` there because a WebSocket upgrade cannot carry a header —
//! and it reaches referrers, proxy logs and browser history. The Rust client
//! sets headers, so nothing is lost by refusing the URL.
//!
//! The cost is stated rather than hidden: a browser `EventSource` cannot set
//! request headers, so a future web client needs a different door. No such
//! client exists — `apps/web` is the localStorage stub ADR-0012 left — and
//! inventing the hazard now for a consumer that does not exist is the wrong
//! trade.
//!
//! # Expiry
//!
//! A session is bounded by its token, not by its own lifetime. `deadline_ms`
//! is the bearer's `exp`, carried here because a long-lived event stream would
//! otherwise outlive the credential that opened it — the exact defect
//! `api/sync.rs`'s `an_expired_token_ends_the_session` covers — the socket
//! suite that first pinned it moved inline beside the operations that replaced
//! the frames when ADR-0023 retired the socket.

use crate::auth::Subject;
use crate::relay::ConnId;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use sunrise_wire_protocol::{HelloAck, SubscribeEntry};

/// How long an idle session survives with no stream attached.
///
/// A session whose client never opened `/sync/events`, or opened it and went
/// away, is a row nobody will collect otherwise: unlike a socket, an HTTP
/// operation has no close to hang cleanup on.
const IDLE_TTL_MS: u64 = 15 * 60 * 1000;

/// One established sync session.
#[derive(Debug, Clone)]
pub struct Session {
    /// Hashed account id — the relay channel namespace for this session.
    pub account: [u8; 16],
    /// The account these operations act on.
    pub account_id: String,
    /// `(iss, sub)`, fixed for the life of the session. A refresh presenting a
    /// different principal is refused rather than accepted.
    pub subject: Subject,
    /// The device bound at establishment, when one was.
    pub device_id: Option<String>,
    /// The bearer's `exp`. `None` only for the self-host verifier, which has no
    /// IdP and therefore no token to age out.
    pub deadline_ms: Option<u64>,
    /// This session's relay connection id, so its own frames are not echoed
    /// back to it.
    pub conn: ConnId,
    /// What `Hello` negotiated. Returned once and then fixed.
    pub negotiated: HelloAck,
    /// The streams this session is currently subscribed to, with cursors.
    ///
    /// Replaced wholesale by `POST /sync/subscribe`, never merged — the same
    /// semantics a re-sent `Subscribe` frame had, where a second subscription
    /// for one stream replaced the first rather than duplicating it.
    pub streams: Vec<SubscribeEntry>,
    /// When this session was last touched, for idle collection.
    pub seen_ms: u64,
}

impl Session {
    /// Whether `now_ms` is past the token's expiry.
    #[must_use]
    pub fn expired(&self, now_ms: u64) -> bool {
        self.deadline_ms.is_some_and(|d| now_ms >= d)
    }
}

/// Every live session, keyed by id.
#[derive(Debug, Clone, Default)]
pub struct SessionStore {
    inner: Arc<Mutex<HashMap<String, Session>>>,
}

impl SessionStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// File `session` under a fresh id and return it.
    ///
    /// The id is 16 random bytes: it names a live stream of one account's ops,
    /// so it is guessed rather than enumerated only if it carries real entropy.
    pub fn insert(&self, session: Session) -> Result<String, getrandom::Error> {
        let mut raw = [0u8; 16];
        getrandom::getrandom(&mut raw)?;
        let id = format!("ses_{}", hex::encode(raw));
        self.inner.lock().insert(id.clone(), session);
        Ok(id)
    }

    /// The session `id` names, if it is live and unexpired.
    ///
    /// Collects the expired row on the way past rather than leaving it for a
    /// sweep: the read is where expiry is noticed, so it is the cheapest place
    /// to act on it.
    pub fn get(&self, id: &str, now_ms: u64) -> Option<Session> {
        let mut sessions = self.inner.lock();
        match sessions.get(id) {
            Some(s) if s.expired(now_ms) => {
                sessions.remove(id);
                None
            }
            Some(s) => Some(s.clone()),
            None => None,
        }
    }

    /// Apply `f` to the session `id` names, if it is live and unexpired.
    pub fn update<F: FnOnce(&mut Session)>(&self, id: &str, now_ms: u64, f: F) -> bool {
        let mut sessions = self.inner.lock();
        match sessions.get_mut(id) {
            Some(s) if s.expired(now_ms) => {
                sessions.remove(id);
                false
            }
            Some(s) => {
                s.seen_ms = now_ms;
                f(s);
                true
            }
            None => false,
        }
    }

    /// Drop `id`.
    pub fn remove(&self, id: &str) {
        self.inner.lock().remove(id);
    }

    /// Drop every expired or long-idle session.
    ///
    /// Called on establishment, which is the one operation guaranteed to keep
    /// happening on a live server: hanging collection off the busiest path
    /// avoids a timer task whose only job is to hold a lock occasionally.
    pub fn collect(&self, now_ms: u64) {
        self.inner
            .lock()
            .retain(|_, s| !s.expired(now_ms) && now_ms.saturating_sub(s.seen_ms) < IDLE_TTL_MS);
    }

    /// How many sessions are live. For the metrics exposition and for tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// Whether no session is live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::{Session, SessionStore, IDLE_TTL_MS};
    use crate::auth::Subject;
    use sunrise_wire_protocol::HelloAck;

    fn session(deadline_ms: Option<u64>, seen_ms: u64) -> Session {
        Session {
            account: [1u8; 16],
            account_id: "acc".to_owned(),
            subject: Subject::new("iss", "sub"),
            device_id: None,
            deadline_ms,
            conn: 1,
            negotiated: HelloAck {
                server_app_v: "0".to_owned(),
                wire_proto: 1,
                crypto_suite: 1,
                doc_schema_floor: 1,
                capabilities: 0,
                server_time_ms: 0,
            },
            streams: Vec::new(),
            seen_ms,
        }
    }

    /// A session outliving its own token is the defect the deadline exists for.
    #[test]
    fn an_expired_session_is_not_readable_and_is_collected() {
        let store = SessionStore::new();
        let id = store.insert(session(Some(1_000), 0)).unwrap();

        assert!(store.get(&id, 999).is_some(), "live before the deadline");
        assert!(store.get(&id, 1_000).is_none(), "gone at the deadline");
        assert!(store.is_empty(), "and collected rather than left behind");
    }

    /// Self-host has no IdP and therefore no token to age out.
    #[test]
    fn a_session_with_no_deadline_never_expires() {
        let store = SessionStore::new();
        let id = store.insert(session(None, 0)).unwrap();
        assert!(store.get(&id, u64::MAX).is_some());
    }

    /// An HTTP operation has no close to hang cleanup on, so an abandoned
    /// session has to age out on its own.
    #[test]
    fn an_idle_session_is_collected() {
        let store = SessionStore::new();
        let _ = store.insert(session(None, 0)).unwrap();
        store.collect(IDLE_TTL_MS - 1);
        assert_eq!(store.len(), 1, "still within the idle window");
        store.collect(IDLE_TTL_MS);
        assert!(store.is_empty(), "collected once past it");
    }
}

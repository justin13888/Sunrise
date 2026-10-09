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
//! `api/sync/suite.rs`'s `an_expired_token_ends_the_session` covers — the socket
//! suite that first pinned it moved inline beside the operations that replaced
//! the frames when ADR-0023 retired the socket.
//!
//! # Where sessions live
//!
//! Behind [`SessionBackend`], so a relay running as several nodes can keep
//! them in one shared table and honour a session on a node other than the one
//! that opened it (ADR-0062 §3). [`MemorySessions`] is the only backend built:
//! a single process, and every session lost on restart, as before the seam.
//! [`SessionStore`] is what the handlers hold either way.

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
    /// Whether a `Subscribe` has replaced the stream set and no
    /// `GET /sync/events` has served those cursors yet.
    ///
    /// This is what makes the resume order a server property rather than a
    /// client convention. A `Last-Event-ID` says "I *received* everything
    /// through this frame"; the cursors in a `Subscribe` say "I have
    /// *applied* everything through these seqs", and the two diverge exactly
    /// when delivery succeeded and application did not — which is the state a
    /// client re-sends `Subscribe` to get out of. While this flag is set, a
    /// resume id is an older statement than the cursors beside it, so
    /// [`crate::api::sync::events`] refuses the pair with
    /// `SYNC_RESUME_CONFLICT` instead of silently honouring one and dropping
    /// the frames the other asked for.
    ///
    /// Cleared by the stream that serves the set, so an ordinary reconnect —
    /// a stream that follows another stream rather than a `Subscribe` —
    /// resumes on its id as before.
    pub subscribe_unserved: bool,
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

/// Why a session operation could not be carried out.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// No entropy for a fresh session id.
    #[error("could not draw a session id: {0}")]
    Entropy(#[from] getrandom::Error),
    /// The backend holding sessions did not answer. Never raised by
    /// [`MemorySessions`]; a shared backend raises it for a lost connection or
    /// a failed statement, and the caller answers `503`, which is retryable.
    #[error("session store unavailable: {0}")]
    Unavailable(String),
}

/// One change to a stored session, applied by [`SessionBackend::update`].
pub type SessionEdit = Box<dyn FnOnce(&mut Session) + Send>;

/// Where live sessions are kept: the seam ADR-0062 §3 puts a shared table
/// behind, so a session opened on one relay node is honoured on another.
///
/// [`MemorySessions`] is the single-node implementation and the only one
/// today. Async because the store a cluster shares is across a network, as
/// [`crate::api::ratelimit::store::LimiterStore`] is; the in-process one never
/// awaits. Every method is fallible for the same reason, so a shared backend
/// adds no signature change.
///
/// What every implementation owes, and the conformance suite in this module
/// checks:
///
/// - An expired session is never returned or updated, and the read or update
///   that finds it expired also removes it.
/// - [`update`](Self::update) stamps `seen_ms` with `now_ms` before applying
///   the edit, so a session in use is never idle-collected.
/// - [`collect`](Self::collect) drops every expired session and every one idle
///   for the idle bound (15 minutes) or longer, and nothing else.
#[async_trait::async_trait]
pub trait SessionBackend: Send + Sync + std::fmt::Debug {
    /// File `session` under `id`. The id is drawn by [`SessionStore`].
    async fn insert(&self, id: String, session: Session) -> Result<(), SessionError>;

    /// The session `id` names, if it is live and unexpired.
    async fn get(&self, id: &str, now_ms: u64) -> Result<Option<Session>, SessionError>;

    /// Apply `edit` to the session `id` names, if it is live and unexpired.
    /// Returns whether one was.
    async fn update(&self, id: &str, now_ms: u64, edit: SessionEdit) -> Result<bool, SessionError>;

    /// Drop `id`. Dropping an absent id is not an error.
    async fn remove(&self, id: &str) -> Result<(), SessionError>;

    /// Drop every expired or long-idle session.
    async fn collect(&self, now_ms: u64) -> Result<(), SessionError>;

    /// How many sessions are held, collected or not.
    async fn count(&self) -> Result<usize, SessionError>;
}

/// The in-process [`SessionBackend`]: one lock over one map.
#[derive(Debug, Default)]
pub struct MemorySessions {
    inner: Mutex<HashMap<String, Session>>,
}

impl MemorySessions {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl SessionBackend for MemorySessions {
    async fn insert(&self, id: String, session: Session) -> Result<(), SessionError> {
        self.inner.lock().insert(id, session);
        Ok(())
    }

    /// Collects the expired row on the way past rather than leaving it for a
    /// sweep: the read is where expiry is noticed, so it is the cheapest place
    /// to act on it.
    async fn get(&self, id: &str, now_ms: u64) -> Result<Option<Session>, SessionError> {
        let mut sessions = self.inner.lock();
        Ok(match sessions.get(id) {
            Some(s) if s.expired(now_ms) => {
                sessions.remove(id);
                None
            }
            Some(s) => Some(s.clone()),
            None => None,
        })
    }

    async fn update(&self, id: &str, now_ms: u64, edit: SessionEdit) -> Result<bool, SessionError> {
        let mut sessions = self.inner.lock();
        Ok(match sessions.get_mut(id) {
            Some(s) if s.expired(now_ms) => {
                sessions.remove(id);
                false
            }
            Some(s) => {
                s.seen_ms = now_ms;
                edit(s);
                true
            }
            None => false,
        })
    }

    async fn remove(&self, id: &str) -> Result<(), SessionError> {
        self.inner.lock().remove(id);
        Ok(())
    }

    async fn collect(&self, now_ms: u64) -> Result<(), SessionError> {
        self.inner
            .lock()
            .retain(|_, s| !s.expired(now_ms) && now_ms.saturating_sub(s.seen_ms) < IDLE_TTL_MS);
        Ok(())
    }

    async fn count(&self) -> Result<usize, SessionError> {
        Ok(self.inner.lock().len())
    }
}

/// Every live session, keyed by id: the handle the handlers hold. Cheap to
/// clone (`Arc` inside).
///
/// It draws the id, which is the one rule no backend may weaken, and logs a
/// backend failure once, here, so each call site only has to decide what the
/// failure means for its own response.
#[derive(Debug, Clone)]
pub struct SessionStore {
    backend: Arc<dyn SessionBackend>,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore {
    /// An empty in-process store.
    #[must_use]
    pub fn new() -> Self {
        Self::with_backend(Arc::new(MemorySessions::new()))
    }

    /// A store over `backend`.
    #[must_use]
    pub fn with_backend(backend: Arc<dyn SessionBackend>) -> Self {
        Self { backend }
    }

    /// File `session` under a fresh id and return it.
    ///
    /// The id is 16 random bytes: it names a live stream of one account's ops,
    /// so it is guessed rather than enumerated only if it carries real entropy.
    pub async fn insert(&self, session: Session) -> Result<String, SessionError> {
        let mut raw = [0u8; 16];
        getrandom::getrandom(&mut raw)?;
        let id = format!("ses_{}", hex::encode(raw));
        logged(self.backend.insert(id.clone(), session).await)?;
        Ok(id)
    }

    /// The session `id` names, if it is live and unexpired.
    pub async fn get(&self, id: &str, now_ms: u64) -> Result<Option<Session>, SessionError> {
        logged(self.backend.get(id, now_ms).await)
    }

    /// Apply `f` to the session `id` names, if it is live and unexpired.
    /// Returns whether one was.
    pub async fn update<F>(&self, id: &str, now_ms: u64, f: F) -> Result<bool, SessionError>
    where
        F: FnOnce(&mut Session) + Send + 'static,
    {
        logged(self.backend.update(id, now_ms, Box::new(f)).await)
    }

    /// Drop `id`.
    pub async fn remove(&self, id: &str) -> Result<(), SessionError> {
        logged(self.backend.remove(id).await)
    }

    /// Drop every expired or long-idle session.
    ///
    /// Called on establishment, which is the one operation guaranteed to keep
    /// happening on a live server: hanging collection off the busiest path
    /// avoids a timer task whose only job is to hold a lock occasionally.
    pub async fn collect(&self, now_ms: u64) -> Result<(), SessionError> {
        logged(self.backend.collect(now_ms).await)
    }

    /// How many sessions are held. For the metrics exposition and for tests.
    pub async fn len(&self) -> Result<usize, SessionError> {
        logged(self.backend.count().await)
    }

    /// Whether no session is held.
    pub async fn is_empty(&self) -> Result<bool, SessionError> {
        Ok(self.len().await? == 0)
    }
}

/// Log a backend failure, once, where every call site passes through.
fn logged<T>(result: Result<T, SessionError>) -> Result<T, SessionError> {
    if let Err(e) = &result {
        tracing::error!(
            ev = "srv.sync.session_store_failed",
            err_code = %sunrise_error::ErrorCode::RelayStorageUnavailable,
            err_kind = "transient",
            retryable = true,
            cause = %e,
            "the session store did not answer"
        );
    }
    result
}

impl From<SessionError> for crate::api::error::ApiError {
    /// An id that could not be drawn is the server's own failure, as it was
    /// before the seam; a backend that did not answer is `503`, so the client
    /// retries rather than reading it as a refusal.
    fn from(e: SessionError) -> Self {
        match e {
            SessionError::Entropy(_) => Self::internal(),
            SessionError::Unavailable(_) => Self::unavailable("the session store is unavailable"),
        }
    }
}

/// The checks every [`SessionBackend`] must pass, written once and run against
/// each implementation: [`MemorySessions`] in this module's tests, and a
/// shared backend in its own. Also the backend that fails every call, for the
/// tests of what a caller does with a failure.
#[cfg(test)]
pub(crate) mod conformance {
    use super::{Session, SessionBackend, SessionEdit, SessionError, IDLE_TTL_MS};

    /// A backend that never answers, standing in for a shared one that lost
    /// its connection.
    #[derive(Debug)]
    pub(crate) struct Unreachable;

    #[async_trait::async_trait]
    impl SessionBackend for Unreachable {
        async fn insert(&self, _: String, _: Session) -> Result<(), SessionError> {
            Err(down())
        }
        async fn get(&self, _: &str, _: u64) -> Result<Option<Session>, SessionError> {
            Err(down())
        }
        async fn update(&self, _: &str, _: u64, _: SessionEdit) -> Result<bool, SessionError> {
            Err(down())
        }
        async fn remove(&self, _: &str) -> Result<(), SessionError> {
            Err(down())
        }
        async fn collect(&self, _: u64) -> Result<(), SessionError> {
            Err(down())
        }
        async fn count(&self) -> Result<usize, SessionError> {
            Err(down())
        }
    }

    /// The in-process backend with a switch per call that can fail, for the
    /// failures that only arrive after a session exists: a `remove` or an
    /// `update` refused, or a `get` lost mid-stream. [`Unreachable`] cannot
    /// reach those, since `resolve` fails before them.
    #[derive(Debug, Default)]
    pub(crate) struct Faulty {
        inner: super::MemorySessions,
        /// `get` fails while set.
        pub(crate) get: std::sync::atomic::AtomicBool,
        /// `update` fails while set.
        pub(crate) update: std::sync::atomic::AtomicBool,
        /// `remove` fails while set.
        pub(crate) remove: std::sync::atomic::AtomicBool,
    }

    impl Faulty {
        fn check(flag: &std::sync::atomic::AtomicBool) -> Result<(), SessionError> {
            if flag.load(std::sync::atomic::Ordering::SeqCst) {
                Err(down())
            } else {
                Ok(())
            }
        }
    }

    #[async_trait::async_trait]
    impl SessionBackend for Faulty {
        async fn insert(&self, id: String, session: Session) -> Result<(), SessionError> {
            self.inner.insert(id, session).await
        }
        async fn get(&self, id: &str, now_ms: u64) -> Result<Option<Session>, SessionError> {
            Self::check(&self.get)?;
            self.inner.get(id, now_ms).await
        }
        async fn update(
            &self,
            id: &str,
            now_ms: u64,
            edit: SessionEdit,
        ) -> Result<bool, SessionError> {
            Self::check(&self.update)?;
            self.inner.update(id, now_ms, edit).await
        }
        async fn remove(&self, id: &str) -> Result<(), SessionError> {
            Self::check(&self.remove)?;
            self.inner.remove(id).await
        }
        async fn collect(&self, now_ms: u64) -> Result<(), SessionError> {
            self.inner.collect(now_ms).await
        }
        async fn count(&self) -> Result<usize, SessionError> {
            self.inner.count().await
        }
    }

    fn down() -> SessionError {
        SessionError::Unavailable("connection refused".to_owned())
    }

    /// Run every check against a backend that starts empty.
    pub(crate) async fn run(
        backend: &dyn SessionBackend,
        session: impl Fn(Option<u64>, u64) -> Session,
    ) {
        // An expired session is not readable, and is collected by the read.
        backend
            .insert("a".into(), session(Some(1_000), 0))
            .await
            .unwrap();
        assert!(
            backend.get("a", 999).await.unwrap().is_some(),
            "live before the deadline"
        );
        assert!(
            backend.get("a", 1_000).await.unwrap().is_none(),
            "gone at the deadline"
        );
        assert_eq!(
            backend.count().await.unwrap(),
            0,
            "and collected, not left behind"
        );

        // Nor updatable, and the update collects it too.
        backend
            .insert("b".into(), session(Some(1_000), 0))
            .await
            .unwrap();
        let touched = backend
            .update("b", 1_000, Box::new(|s| s.subscribe_unserved = true))
            .await
            .unwrap();
        assert!(!touched, "an expired session is not updated");
        assert_eq!(backend.count().await.unwrap(), 0);

        // No deadline is no expiry.
        backend.insert("c".into(), session(None, 0)).await.unwrap();
        assert!(backend.get("c", u64::MAX).await.unwrap().is_some());

        // An update stamps `seen_ms` and applies the edit, and the edit is what
        // the next read sees.
        let touched = backend
            .update("c", 500, Box::new(|s| s.subscribe_unserved = true))
            .await
            .unwrap();
        assert!(touched);
        let read = backend.get("c", 500).await.unwrap().unwrap();
        assert!(read.subscribe_unserved);
        assert_eq!(read.seen_ms, 500);
        assert!(
            !backend
                .update("absent", 500, Box::new(|_| {}))
                .await
                .unwrap(),
            "an absent id is reported, not created"
        );

        // Idle collection keeps a session inside the window and drops it at
        // the window, measured from the last touch.
        backend.collect(500 + IDLE_TTL_MS - 1).await.unwrap();
        assert_eq!(
            backend.count().await.unwrap(),
            1,
            "still within the idle window"
        );
        backend.collect(500 + IDLE_TTL_MS).await.unwrap();
        assert_eq!(backend.count().await.unwrap(), 0, "collected once past it");

        // Collection also takes the expired, and leaves the live.
        backend
            .insert("d".into(), session(Some(10), 0))
            .await
            .unwrap();
        backend.insert("e".into(), session(None, 0)).await.unwrap();
        backend.collect(10).await.unwrap();
        assert!(backend.get("e", 10).await.unwrap().is_some());
        assert_eq!(backend.count().await.unwrap(), 1);

        // Removal is idempotent.
        backend.remove("e").await.unwrap();
        backend.remove("e").await.unwrap();
        assert_eq!(backend.count().await.unwrap(), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::conformance::{self, Unreachable};
    use super::{MemorySessions, Session, SessionError, SessionStore, IDLE_TTL_MS};
    use crate::api::error::{codes, ApiError};
    use crate::auth::Subject;
    use std::sync::Arc;
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
            subscribe_unserved: false,
            seen_ms,
        }
    }

    /// The in-process backend owes everything any backend owes.
    #[tokio::test]
    async fn the_memory_backend_passes_the_conformance_suite() {
        conformance::run(&MemorySessions::new(), session).await;
    }

    /// A session outliving its own token is the defect the deadline exists for.
    #[tokio::test]
    async fn an_expired_session_is_not_readable_and_is_collected() {
        let store = SessionStore::new();
        let id = store.insert(session(Some(1_000), 0)).await.unwrap();

        assert!(
            store.get(&id, 999).await.unwrap().is_some(),
            "live before the deadline"
        );
        assert!(
            store.get(&id, 1_000).await.unwrap().is_none(),
            "gone at the deadline"
        );
        assert!(
            store.is_empty().await.unwrap(),
            "and collected rather than left behind"
        );
    }

    /// Self-host has no IdP and therefore no token to age out.
    #[tokio::test]
    async fn a_session_with_no_deadline_never_expires() {
        let store = SessionStore::new();
        let id = store.insert(session(None, 0)).await.unwrap();
        assert!(store.get(&id, u64::MAX).await.unwrap().is_some());
    }

    /// An HTTP operation has no close to hang cleanup on, so an abandoned
    /// session has to age out on its own.
    #[tokio::test]
    async fn an_idle_session_is_collected() {
        let store = SessionStore::new();
        let _ = store.insert(session(None, 0)).await.unwrap();
        store.collect(IDLE_TTL_MS - 1).await.unwrap();
        assert_eq!(
            store.len().await.unwrap(),
            1,
            "still within the idle window"
        );
        store.collect(IDLE_TTL_MS).await.unwrap();
        assert!(store.is_empty().await.unwrap(), "collected once past it");
    }

    /// The id is drawn by the handle, not the backend, so no backend can
    /// weaken it: `ses_` and 32 lowercase hex digits, fresh each time.
    #[tokio::test]
    async fn the_handle_draws_a_fresh_full_entropy_id() {
        let store = SessionStore::new();
        let a = store.insert(session(None, 0)).await.unwrap();
        let b = store.insert(session(None, 0)).await.unwrap();
        for id in [&a, &b] {
            let hex = id.strip_prefix("ses_").unwrap();
            assert_eq!(hex.len(), 32);
            assert!(hex.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')));
        }
        assert_ne!(a, b);
    }

    /// A backend failure reaches the caller typed and logged, and becomes a
    /// retryable `503` rather than the `401` an absent session would be.
    #[test]
    fn a_backend_failure_is_logged_and_answers_503() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let store = SessionStore::with_backend(Arc::new(Unreachable));
        let events = sunrise_log::test_util::events_emitted_by(|| {
            assert!(matches!(
                rt.block_on(store.get("ses_x", 0)),
                Err(SessionError::Unavailable(_))
            ));
        });
        assert!(events
            .iter()
            .any(|ev| ev == "srv.sync.session_store_failed"));

        let api: ApiError = rt
            .block_on(store.insert(session(None, 0)))
            .unwrap_err()
            .into();
        assert!(
            matches!(api, ApiError::Unavailable { code, .. } if code == codes::RELAY_STORAGE_UNAVAILABLE),
            "{api:?}"
        );
    }
}

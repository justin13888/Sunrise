//! The bearer a sync session presents, and where it lives.
//!
//! The relay authenticates **every sync operation** (`docs/06-server/auth.md`):
//! each `POST` and the event stream carries `Authorization: Bearer`, so every
//! reconnect needs a token — and it needs the *current* one. A sync
//! session outlives many tokens: it renews in-band with `0x12 RefreshToken`
//! while connected, and after a drop it reconnects with whatever the OIDC
//! client has obtained since.
//!
//! That rules out passing a `String` into the transport factory. The factory
//! closure is built once and called on every connect attempt for the life of
//! the process, so a captured `String` is frozen at the value it had when sync
//! started — correct for exactly one hour, and then permanently wrong in a way
//! that presents as "reconnects stopped working".
//!
//! [`TokenSource`] is the indirection: one shared cell the login flow writes
//! and the factory reads.

use std::sync::{Arc, RwLock};

use tokio::sync::watch;

/// A shared, swappable bearer token.
///
/// Cheap to clone — clones share one cell, which is the point. Cloning into a
/// factory closure and then updating the original is the supported pattern.
///
/// A write also **wakes every [`TokenWatch`]**. That is what lets a renewal
/// reach a *live* session: the sync driver sends the new token in a
/// `0x12 RefreshToken` frame rather than waiting for the relay to close the
/// session and reconnecting.
#[derive(Clone)]
pub struct TokenSource(Arc<Inner>);

struct Inner {
    token: RwLock<Option<String>>,
    /// Version, published through a `watch` channel.
    ///
    /// A `watch` rather than a `Notify`, and the difference is the whole
    /// mechanism working or not. `Notify::notify_waiters` wakes whoever is
    /// parked *at that instant*; a session loop is not parked while it is
    /// handling an event, so a renewal landing in that window wakes nobody and
    /// the relay keeps the stale credential until it expires — which is to say
    /// the feature silently does nothing under exactly the load it exists for.
    /// A `watch` is level-triggered against each receiver's own last-seen
    /// version, so a write cannot be missed however the timing falls.
    version: watch::Sender<u64>,
}

impl Default for TokenSource {
    fn default() -> Self {
        Self::new(None)
    }
}

impl TokenSource {
    /// A source holding `token`, or nothing.
    #[must_use]
    pub fn new(token: Option<String>) -> Self {
        Self(Arc::new(Inner {
            token: RwLock::new(token),
            version: watch::Sender::new(0),
        }))
    }

    /// A source holding no token. A connect through this is unauthenticated
    /// and only a self-host relay will accept it.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(None)
    }

    /// The current token, if any.
    ///
    /// A poisoned lock reads as "no token" rather than panicking: a panicking
    /// writer must not make the process unable to sync, and an absent token
    /// fails closed at the relay's `401`.
    #[must_use]
    pub fn get(&self) -> Option<String> {
        self.0.token.read().map_or(None, |g| g.clone())
    }

    /// Replace the token and wake every [`TokenWatch`].
    ///
    /// A live session sends the new bearer in-band; the next reconnect reads
    /// it from here. Neither path requires the driver to be restarted.
    pub fn set(&self, token: Option<String>) {
        if let Ok(mut g) = self.0.token.write() {
            *g = token;
        }
        self.0.version.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// How many times the token has been replaced.
    #[must_use]
    pub fn version(&self) -> u64 {
        *self.0.version.borrow()
    }

    /// A handle that resolves whenever the token is replaced.
    ///
    /// Take one per consumer and keep it: it remembers which version that
    /// consumer has acted on, which is what makes a write impossible to miss.
    #[must_use]
    pub fn watch(&self) -> TokenWatch {
        TokenWatch(self.0.version.subscribe())
    }

    /// Whether a token is present.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.get().is_some()
    }
}

/// A consumer's view of [`TokenSource`] changes.
///
/// Level-triggered: [`TokenWatch::changed`] resolves for any write this handle
/// has not yet observed, whether or not it happened to be parked when the write
/// landed.
#[derive(Debug, Clone)]
pub struct TokenWatch(watch::Receiver<u64>);

impl TokenWatch {
    /// Wait until the token has been replaced since this handle last observed
    /// it, and return the new version.
    ///
    /// Cancel-safe, so it can sit in a `tokio::select!` arm beside a session's
    /// other deadlines. When the source is gone it never resolves rather than
    /// resolving forever — a dropped source means no renewal is coming, and a
    /// ready arm would spin the session loop.
    pub async fn changed(&mut self) -> u64 {
        if self.0.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        *self.0.borrow_and_update()
    }

    /// The version this handle has observed.
    #[must_use]
    pub fn seen(&self) -> u64 {
        *self.0.borrow()
    }
}

// Hand-written: a derived `Debug` would print the bearer, and this type is
// reachable from `SyncConfig`, which is reachable from `CoreConfig`, which is
// `Debug` and gets logged.
impl std::fmt::Debug for TokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSource")
            .field("set", &self.is_set())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clone_sees_a_later_write() {
        let a = TokenSource::new(Some("first".into()));
        let b = a.clone();
        assert_eq!(b.get().as_deref(), Some("first"));
        a.set(Some("second".into()));
        assert_eq!(
            b.get().as_deref(),
            Some("second"),
            "a factory closure holding a clone must see the renewed token"
        );
    }

    #[test]
    fn clearing_leaves_nothing_behind() {
        let s = TokenSource::new(Some("tok".into()));
        s.set(None);
        assert!(!s.is_set());
        assert_eq!(s.get(), None);
    }

    #[test]
    fn a_write_bumps_the_version() {
        let s = TokenSource::new(None);
        assert_eq!(s.version(), 0);
        s.set(Some("a".into()));
        assert_eq!(s.version(), 1);
        s.set(None);
        assert_eq!(s.version(), 2);
    }

    #[tokio::test]
    async fn a_write_wakes_a_parked_watcher() {
        let s = TokenSource::new(None);
        let mut w = s.watch();
        let waiter = tokio::spawn(async move { w.changed().await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        s.set(Some("renewed".into()));
        assert_eq!(waiter.await.unwrap(), 1);
    }

    /// **The regression this type exists to prevent.** A write that lands
    /// while the consumer is busy — not parked on `changed()` — must still be
    /// observed. Under the `Notify` this started as, it was not: the wake went
    /// to nobody, the session loop stayed parked on its 30-second anti-entropy
    /// timer, and the relay kept the stale credential until it expired. It
    /// presented as a renewal that simply never happened.
    #[tokio::test]
    async fn a_write_while_the_watcher_is_busy_is_still_observed() {
        let s = TokenSource::new(None);
        let mut w = s.watch();
        // The consumer is doing something else entirely when the write lands.
        s.set(Some("renewed".into()));
        let version = tokio::time::timeout(std::time::Duration::from_secs(2), w.changed())
            .await
            .expect("a write nobody was parked for must still wake the next wait");
        assert_eq!(version, 1);
        assert_eq!(s.get().as_deref(), Some("renewed"));
    }

    /// And it does not fire twice for one write.
    #[tokio::test]
    async fn an_observed_write_does_not_wake_again() {
        let s = TokenSource::new(None);
        let mut w = s.watch();
        s.set(Some("a".into()));
        assert_eq!(w.changed().await, 1);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), w.changed())
                .await
                .is_err(),
            "no further write happened"
        );
    }

    /// The token is a live credential and this type is reachable from
    /// `CoreConfig`, which is `Debug` and does get logged.
    #[test]
    fn debug_never_prints_the_token() {
        let s = TokenSource::new(Some("super-secret-bearer".into()));
        let rendered = format!("{s:?}");
        assert!(!rendered.contains("super-secret-bearer"), "{rendered}");
        assert!(rendered.contains("set: true"), "{rendered}");
    }
}

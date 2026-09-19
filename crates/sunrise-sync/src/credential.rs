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
    /// Take one per consumer and keep it: [`TokenWatch::changed`] remembers
    /// which version it last woke that consumer for, which is what makes a
    /// write impossible to miss. That memory is private to `changed` — it is
    /// the receiver's own position in the channel, and nothing reads it out as
    /// a number. [`TokenWatch::mark_current`] is the way to move that position,
    /// and the only operation that can say whether a given handle was behind.
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

    /// Bring this handle forward to whatever the source has already sent, and
    /// report whether it was behind.
    ///
    /// `Some(v)` when this handle had fallen behind and has now been brought
    /// forward to version `v`; `None` when it was already current and there was
    /// nothing to consume. Either way [`TokenWatch::changed`] afterwards waits
    /// for the next *write* rather than resolving immediately for writes that
    /// landed earlier.
    ///
    /// It exists for a consumer that has just read the token by another route
    /// and so is already holding the latest bearer — a sync session whose
    /// connect read the credential after those writes landed. Announcing them
    /// again would be work the relay does not need, because the connect
    /// already carried them.
    ///
    /// # Why the answer cannot be a version number
    ///
    /// The value in the watch cell *is* the source's counter, the same number
    /// [`TokenSource::version`] returns, so every handle reads the same value
    /// at every position. Comparing that against the source, or against what a
    /// previous call returned, therefore cannot report that *this* handle
    /// moved — a guard written either way is dead code rather than a staleness
    /// check, which is the defect #244 reported. The receiver's own position in
    /// the channel is the only thing that distinguishes two handles, nothing
    /// reads it out as a number, and `Ref::has_changed` is what consults it.
    pub fn mark_current(&mut self) -> Option<u64> {
        let current = self.0.borrow_and_update();
        current.has_changed().then(|| *current)
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

    /// A write that landed while this handle was not waiting is consumed
    /// without waking anyone.
    ///
    /// This is the shape the sync driver relies on: writes land while the
    /// client is disconnected, the next connect reads the token and so already
    /// carries them, and the handle is brought forward rather than made to
    /// re-announce what the connect delivered. Built on `borrow` instead of
    /// `borrow_and_update` this leaves the handle where it was, and the wait
    /// below resolves at once.
    #[tokio::test]
    async fn a_handle_that_missed_a_write_is_brought_current() {
        let s = TokenSource::new(None);
        let mut w = s.watch();
        // The offline window: two writes, and nobody waiting on either.
        s.set(Some("a".into()));
        s.set(Some("b".into()));
        assert_eq!(
            w.mark_current(),
            Some(2),
            "the handle was behind, and both writes are consumed and counted"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), w.changed())
                .await
                .is_err(),
            "a renewal the connect already carried must not wake the pump"
        );
    }

    /// And bringing a handle that is already current forward eats nothing.
    ///
    /// The opposite over-correction to the one above: a `mark_current` that
    /// left the receiver marked *past* the sender would swallow the next real
    /// renewal, which is the failure `TokenWatch` exists to prevent.
    #[tokio::test]
    async fn bringing_a_current_handle_forward_swallows_nothing() {
        let s = TokenSource::new(None);
        let mut w = s.watch();
        assert_eq!(
            w.mark_current(),
            None,
            "a fresh handle is already current and has nothing to consume"
        );
        s.set(Some("a".into()));
        assert_eq!(w.changed().await, 1, "the next real write still arrives");
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

    /// **The falsifier for #244's defect.** A handle brought forward by
    /// `changed` is already current, and the mark must say so.
    ///
    /// `mark_current` used to return `*borrow_and_update()` — the *source's*
    /// counter, which is the same number for every handle at every position.
    /// Here that return was `1` having consumed nothing, and a caller comparing
    /// it against the number a previous attempt saw read it as "this connect
    /// brought the handle forward". The source's counter is deliberately left
    /// non-zero below, because that is what made the old return look right.
    #[tokio::test]
    async fn a_handle_already_brought_forward_by_changed_consumes_nothing() {
        let s = TokenSource::new(None);
        let mut w = s.watch();
        s.set(Some("a".into()));
        assert_eq!(w.changed().await, 1, "the write wakes the parked handle");
        assert_eq!(
            w.mark_current(),
            None,
            "`changed` already brought this handle forward; the mark consumed nothing"
        );
        assert_eq!(
            s.version(),
            1,
            "and the source's counter is not zero, which is what the old return reported"
        );
    }
}

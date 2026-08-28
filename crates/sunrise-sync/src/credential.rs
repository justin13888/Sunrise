//! The bearer a sync session presents, and where it lives.
//!
//! The relay authenticates at the **upgrade** (`docs/06-server/auth.md`), so
//! every reconnect needs a token — and it needs the *current* one. A sync
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

/// A shared, swappable bearer token.
///
/// Cheap to clone — clones share one cell, which is the point. Cloning into a
/// factory closure and then updating the original is the supported pattern.
#[derive(Clone, Default)]
pub struct TokenSource(Arc<RwLock<Option<String>>>);

impl TokenSource {
    /// A source holding `token`, or nothing.
    #[must_use]
    pub fn new(token: Option<String>) -> Self {
        Self(Arc::new(RwLock::new(token)))
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
        self.0.read().map_or(None, |g| g.clone())
    }

    /// Replace the token. Takes effect on the next read — an established
    /// session is renewed in-band, not by reconnecting.
    pub fn set(&self, token: Option<String>) {
        if let Ok(mut g) = self.0.write() {
            *g = token;
        }
    }

    /// Whether a token is present.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.get().is_some()
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

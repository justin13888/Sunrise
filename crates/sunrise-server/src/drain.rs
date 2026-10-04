//! Whether the server is draining, shared by everything that has to react.
//!
//! kynos owns the accept loop and its own drain: once the shutdown trigger
//! resolves it stops accepting and waits for in-flight requests. Two things it
//! cannot know are the server's to do:
//!
//! - **Readiness.** `GET /api/v1/health?deep=1` must answer `503` from the
//!   moment the drain begins, so a load balancer stops routing here before the
//!   listener goes away.
//! - **The SSE streams.** A sync stream is a response that never finishes on
//!   its own, so kynos's drain would wait on every open one until the deadline
//!   and then cut it with no terminal event. Each stream instead watches this
//!   signal, sends a retryable `closed` event, and ends, which lets the drain
//!   complete as soon as the ordinary requests have.
//!
//! [`Drain`] is that signal: one flag set once, observed by every clone of the
//! [`crate::ServerState`] it lives in, plus a count of the open streams so the
//! shutdown log can say how many it ended.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::watch;

/// The process-wide draining flag. Cheap to clone; every clone is the same
/// flag.
#[derive(Debug, Clone)]
pub struct Drain {
    flag: Arc<watch::Sender<bool>>,
    open_streams: Arc<AtomicUsize>,
}

impl Default for Drain {
    fn default() -> Self {
        Self::new()
    }
}

impl Drain {
    /// A flag that is not yet draining.
    #[must_use]
    pub fn new() -> Self {
        let (flag, _) = watch::channel(false);
        Self {
            flag: Arc::new(flag),
            open_streams: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Begin draining. Idempotent: a second call changes nothing and wakes
    /// nobody.
    pub fn begin(&self) {
        self.flag
            .send_if_modified(|draining| !std::mem::replace(draining, true));
    }

    /// Whether [`begin`](Self::begin) has been called.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        *self.flag.borrow()
    }

    /// Resolves once draining has begun, at once if it already has.
    ///
    /// Owns its receiver, so the future can be held across a `select!` loop
    /// and outlive the borrow of `self`.
    pub fn wait(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut rx = self.flag.subscribe();
        async move {
            // `Err` means every sender is gone, which only happens once the
            // whole state has been dropped; nothing is left to wait for then.
            let _ = rx.wait_for(|draining| *draining).await;
        }
    }

    /// Count one open SSE stream until the returned guard drops.
    #[must_use]
    pub fn track_stream(&self) -> StreamGuard {
        self.open_streams.fetch_add(1, Ordering::Relaxed);
        StreamGuard {
            open_streams: Arc::clone(&self.open_streams),
        }
    }

    /// How many SSE streams are open right now.
    #[must_use]
    pub fn open_streams(&self) -> usize {
        self.open_streams.load(Ordering::Relaxed)
    }
}

/// Holds one stream in [`Drain::open_streams`] for as long as it lives.
#[derive(Debug)]
pub struct StreamGuard {
    open_streams: Arc<AtomicUsize>,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.open_streams.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::Drain;

    #[tokio::test]
    async fn wait_resolves_once_draining_begins_and_at_once_after() {
        let drain = Drain::new();
        assert!(!drain.is_draining());
        let waiter = tokio::spawn(drain.wait());
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "nothing has begun the drain yet");

        drain.begin();
        waiter.await.unwrap();
        assert!(drain.is_draining());
        // A stream opened after the drain began must see it immediately.
        drain.wait().await;
        // And a second `begin` is harmless.
        drain.begin();
        assert!(drain.clone().is_draining());
    }

    #[test]
    fn a_stream_is_counted_for_exactly_as_long_as_its_guard_lives() {
        let drain = Drain::new();
        let a = drain.track_stream();
        let b = drain.track_stream();
        assert_eq!(drain.open_streams(), 2);
        drop(a);
        assert_eq!(drain.open_streams(), 1);
        drop(b);
        assert_eq!(drain.open_streams(), 0);
    }
}

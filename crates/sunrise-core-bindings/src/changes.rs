//! The change stream: what [`SunriseCore::subscribe_changes`] delivers, the
//! foreign listener it is delivered to, and the handle that stops it.

use std::sync::{Arc, Mutex};

use tokio::sync::broadcast::error::RecvError;

use crate::SunriseCore;

#[uniffi::export]
impl SunriseCore {
    /// Subscribe to domain changes. Cancel by dropping or cancelling the
    /// returned [`Subscription`].
    pub fn subscribe_changes(&self, listener: Arc<dyn ChangeListener>) -> Arc<Subscription> {
        let mut rx = self.inner.changes();
        let handle = self.rt.spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => listener.on_change(ChangeEvent::from(&ev)),
                    Err(RecvError::Lagged(n)) => listener.on_lagged(n),
                    Err(RecvError::Closed) => {
                        listener.on_closed();
                        break;
                    }
                }
            }
        });
        Arc::new(Subscription {
            handle: Mutex::new(Some(handle)),
        })
    }
}

/// One change to the local vault, addressed by entity.
///
/// Deliberately carries no payload beyond the id: a change notification is a
/// prompt to re-read, not a substitute for reading. Sending the entity would
/// make the stream a second, competing source of truth that can arrive out of
/// order with respect to the queries beside it.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ChangeEvent {
    /// An entity was created.
    Created {
        /// The entity.
        entity: sunrise_id::EntityRef,
    },
    /// An entity was updated.
    Updated {
        /// The entity.
        entity: sunrise_id::EntityRef,
    },
    /// An entity was tombstoned.
    Deleted {
        /// The entity.
        entity: sunrise_id::EntityRef,
    },
    /// An entity's tombstone was compacted away.
    Forgotten {
        /// The entity.
        entity: sunrise_id::EntityRef,
    },
    /// The reader's zone changed and no entity did: every view derived from
    /// the zone has to be read again (`docs/10-cross-cutting/time.md` §7).
    TimeZoneChanged {
        /// The new IANA zone.
        zone: String,
    },
}

impl From<&sunrise_core::DomainEvent> for ChangeEvent {
    fn from(e: &sunrise_core::DomainEvent) -> Self {
        match e {
            sunrise_core::DomainEvent::Created(entity) => Self::Created { entity: *entity },
            sunrise_core::DomainEvent::Updated(entity) => Self::Updated { entity: *entity },
            sunrise_core::DomainEvent::Deleted(entity) => Self::Deleted { entity: *entity },
            sunrise_core::DomainEvent::Forgotten(entity) => Self::Forgotten { entity: *entity },
            sunrise_core::DomainEvent::TimeZoneChanged { zone } => {
                Self::TimeZoneChanged { zone: zone.clone() }
            }
        }
    }
}

/// The foreign side of the change stream.
///
/// # `on_lagged` is not optional
///
/// The channel behind this is a `tokio::sync::broadcast` with a bounded
/// buffer, and it is **lossy under load**. A spike measured `seen = 257,
/// lagged = 4743` pushing 5000 events past a slow consumer — which is exactly
/// what a sync catch-up burst looks like. A listener that implements only
/// [`ChangeListener::on_change`] will therefore show stale data after any
/// burst, silently.
///
/// Treat `on_lagged` as **"re-run every query you are showing"**. It is not
/// telemetry.
#[uniffi::export(with_foreign)]
pub trait ChangeListener: Send + Sync + 'static {
    /// One change arrived. Called from a runtime worker thread, never the
    /// foreign main thread.
    fn on_change(&self, event: ChangeEvent);
    /// `skipped` events were dropped because this listener fell behind.
    /// Re-read; do not try to reconstruct what was missed.
    fn on_lagged(&self, skipped: u64);
    /// The vault closed. No further calls will arrive.
    fn on_closed(&self);
}

/// Cancel handle for [`SunriseCore::subscribe_changes`].
#[derive(uniffi::Object)]
pub struct Subscription {
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[uniffi::export]
impl Subscription {
    /// Stop delivering. Idempotent, and safe to call from any thread.
    ///
    /// Deliberately **sync**: Swift's `AsyncStream.Continuation.onTermination`
    /// is a non-async closure and cannot await, and an async cancel forces a
    /// detached teardown that races with deallocation. `JoinHandle::abort`
    /// needs no reactor, so a sync method is safe here.
    pub fn cancel(&self) {
        if let Ok(mut g) = self.handle.lock() {
            if let Some(h) = g.take() {
                h.abort();
            }
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription").finish_non_exhaustive()
    }
}

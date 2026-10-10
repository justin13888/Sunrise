//! The connection, held for one store operation, under a trace span named for
//! it.
//!
//! Every operation the request paths reach takes the connection through
//! [`Store::tx`], so each is one span — `store.relay_append`,
//! `store.active_device` — whose name is the operation and nothing else: no
//! statement text, no bound value, no row. The span opens before the lock is
//! taken, so a trace shows the wait for a contended connection as well as the
//! work. Outside a traced request the span is a no-op and [`Tx`] is the bare
//! guard.
//!
//! What still takes `conn.lock()` directly is off the request paths: the
//! readiness probe's bounded `try_lock_for`, the shutdown checkpoint, the
//! admin CLI, opening and migrating, and the key and backup operations.

//!
//! The same seam meters a request's time in the store, for
//! `sunrise_db_query_duration_seconds`: [`metered`] runs a request's future
//! with a [`StoreTime`] in scope, and every [`Tx`] taken inside it adds its
//! lifetime, lock wait included, when it drops. Outside that scope — a stream's
//! spawned task, the maintenance pass, the admin CLI — nothing is measured.

use super::Store;
use rusqlite::Connection;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The store time one request accumulated, and how many operations it took.
#[derive(Debug, Default)]
pub struct StoreTime {
    nanos: AtomicU64,
    ops: AtomicU64,
}

impl StoreTime {
    /// The time spent, or `None` when the request never took the store.
    #[must_use]
    pub fn spent(&self) -> Option<Duration> {
        (self.ops.load(Ordering::Relaxed) > 0)
            .then(|| Duration::from_nanos(self.nanos.load(Ordering::Relaxed)))
    }
}

tokio::task_local! {
    static SCOPE: Arc<StoreTime>;
}

/// Run `fut`, accumulating every store operation it takes into the returned
/// [`StoreTime`].
pub async fn metered<F: Future>(fut: F) -> (F::Output, Arc<StoreTime>) {
    let time = Arc::new(StoreTime::default());
    let out = SCOPE.scope(Arc::clone(&time), fut).await;
    (out, time)
}

/// The store's connection for one operation. Fields drop in order, so the lock
/// is released before the span ends.
pub(crate) struct Tx<'a> {
    conn: parking_lot::MutexGuard<'a, Connection>,
    _span: sunrise_telemetry::SpanGuard,
    /// When the operation began and whose request it is part of; `None`
    /// outside a [`metered`] scope.
    meter: Option<(tokio::time::Instant, Arc<StoreTime>)>,
}

impl Drop for Tx<'_> {
    fn drop(&mut self) {
        if let Some((started, time)) = self.meter.take() {
            let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            time.nanos.fetch_add(nanos, Ordering::Relaxed);
            time.ops.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl std::ops::Deref for Tx<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.conn
    }
}

impl std::ops::DerefMut for Tx<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

impl Store {
    /// Take the connection for the operation `name`, a `store.`-prefixed
    /// literal naming the method that calls this.
    pub(crate) fn tx(&self, name: &'static str) -> Tx<'_> {
        let span = sunrise_telemetry::span(name, []);
        // Before the lock, so a contended connection's wait is counted as
        // time the request spent in the store, as the span counts it.
        let meter = SCOPE
            .try_with(Arc::clone)
            .ok()
            .map(|time| (tokio::time::Instant::now(), time));
        Tx {
            conn: self.conn.lock(),
            _span: span,
            meter,
        }
    }
}

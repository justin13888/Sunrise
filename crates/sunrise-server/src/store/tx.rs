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

use super::Store;
use rusqlite::Connection;

/// The store's connection for one operation. Fields drop in order, so the lock
/// is released before the span ends.
pub(crate) struct Tx<'a> {
    conn: parking_lot::MutexGuard<'a, Connection>,
    _span: sunrise_telemetry::SpanGuard,
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
        Tx {
            conn: self.conn.lock(),
            _span: span,
        }
    }
}

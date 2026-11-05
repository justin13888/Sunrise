//! The reads behind the full export (`crate::export`, issue #350): every
//! live record of every stored kind, each family ordered by id.
//!
//! Reads only, through the same row readers every query uses, so an exported
//! record is exactly what a query of it returns.

use super::attachment::read_attachment;
use super::block::{block_row, read_block};
use super::context::read_context;
use super::focus::read_focus_sessions;
use super::ids::blob16;
use super::preferences::read_preferences;
use super::review::read_live_tasks;
use super::routine::read_routines;
use super::stream::read_stream;
use super::{Engine, EngineError};
use crate::export::VaultRecords;
use sunrise_domain::ReviewSnapshot;
use sunrise_storage::Db;

/// The ids of every row of `sql`, a `SELECT <id column> ...` with no
/// parameters.
fn ids(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<[u8; 16]>, EngineError> {
    let mut stmt = conn.prepare(sql)?;
    let raw = stmt
        .query_map([], |r| r.get::<_, Vec<u8>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(raw.iter().map(|r| blob16(r)).collect())
}

impl Engine {
    /// Every live record this replica materializes (`crate::export`).
    ///
    /// Tombstones are left out, and so is the Inbox's stream row, which is
    /// not a Stream entity. The review snapshots are decoded from their stored
    /// body; one that does not decode is skipped, as `ReviewHistory` skips it.
    ///
    /// # Errors
    ///
    /// A storage read error.
    pub fn export_records(&self, db: &Db) -> Result<VaultRecords, EngineError> {
        let conn = db.conn();
        let mut out = VaultRecords {
            tasks: read_live_tasks(conn)?,
            routines: read_routines(conn)?,
            focus_sessions: read_focus_sessions(conn, "ORDER BY s.id ASC", &[])?,
            preferences: read_preferences(conn, &sunrise_domain::PREFERENCES_BYTES)?,
            ..VaultRecords::default()
        };
        let mut views = super::saved_view::read_saved_views(conn)?;
        views.sort_by_key(|v| v.id);
        out.saved_views = views;
        for id in ids(
            conn,
            "SELECT stream_id FROM streams WHERE deleted = 0 ORDER BY stream_id ASC",
        )? {
            // The private domain is not a stream; a vault a build before
            // ADR-0046 folded may hold its placeholder until it is rebuilt.
            if id == sunrise_domain::PRIVATE_DOMAIN_BYTES {
                continue;
            }
            if let Some(s) = read_stream(conn, &id)? {
                out.streams.push(s);
            }
        }
        for id in ids(
            conn,
            "SELECT id FROM contexts WHERE deleted = 0 ORDER BY id ASC",
        )? {
            if let Some(c) = read_context(conn, &id)? {
                out.contexts.push(c);
            }
        }
        for id in ids(
            conn,
            "SELECT id FROM blocks WHERE deleted = 0 ORDER BY id ASC",
        )? {
            if let Some(b) = read_block(conn, &id)? {
                out.blocks.push(block_row(conn, b)?);
            }
        }
        for id in ids(
            conn,
            "SELECT id FROM attachments WHERE deleted = 0 ORDER BY id ASC",
        )? {
            if let Some(a) = read_attachment(conn, &id)? {
                out.attachments.push(a);
            }
        }
        let mut stmt = conn.prepare("SELECT body FROM review_snapshots ORDER BY id ASC")?;
        let bodies = stmt
            .query_map([], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        out.review_snapshots = bodies
            .iter()
            .filter_map(|b| ciborium::de::from_reader::<ReviewSnapshot, _>(&b[..]).ok())
            .collect();
        Ok(out)
    }
}

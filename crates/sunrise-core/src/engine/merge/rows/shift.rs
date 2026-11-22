//! The `shifts` row: the `Shift` projection's writer (issue #1244,
//! ADR-0073 §1).
//!
//! It writes what the merge folded and nothing else.

use crate::engine::lww::LwwStamp;
use rusqlite::{params, Transaction};
use std::collections::BTreeSet;
use sunrise_domain::Shift;
use sunrise_id::EntityRef;

/// A focus set as canonical CBOR, or NULL when empty.
fn encode_set(set: &BTreeSet<EntityRef>) -> rusqlite::Result<Option<Vec<u8>>> {
    if set.is_empty() {
        return Ok(None);
    }
    sunrise_cbor::encode_canonical(set)
        .map(Some)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.to_string().into()))
}

/// Write the merged entity to its row. The row is only ever the merge's
/// projection, so it is written whole.
pub(in crate::engine::merge) fn upsert_shift_row(
    tx: &Transaction<'_>,
    s: &Shift,
    head: &LwwStamp,
) -> rusqlite::Result<()> {
    let extra = if s.unknown.is_empty() {
        None
    } else {
        Some(
            sunrise_cbor::encode_canonical(&s.unknown)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.to_string().into()))?,
        )
    };
    tx.execute(
        "INSERT INTO shifts
         (id, name, starts_at_ms, ends_at_ms, zone, place, primary_streams, primary_contexts,
          deleted, created_at_ms, updated_at_ms, lww_hlc_ms, lww_hlc_logical, lww_seq,
          lww_device, extra)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (id) DO UPDATE SET
            name = excluded.name,
            starts_at_ms = excluded.starts_at_ms,
            ends_at_ms = excluded.ends_at_ms,
            zone = excluded.zone,
            place = excluded.place,
            primary_streams = excluded.primary_streams,
            primary_contexts = excluded.primary_contexts,
            deleted = excluded.deleted,
            updated_at_ms = excluded.updated_at_ms,
            lww_hlc_ms = excluded.lww_hlc_ms,
            lww_hlc_logical = excluded.lww_hlc_logical,
            lww_seq = excluded.lww_seq,
            lww_device = excluded.lww_device,
            extra = excluded.extra",
        params![
            &s.id.bytes()[..],
            s.name,
            s.starts_at.as_millisecond(),
            s.ends_at.map(jiff::Timestamp::as_millisecond),
            s.zone,
            s.place.map(|p| p.bytes().to_vec()),
            encode_set(&s.primary_streams)?,
            encode_set(&s.primary_contexts)?,
            i64::from(s.deleted),
            s.created_at.as_millisecond(),
            s.updated_at.as_millisecond(),
            i64::try_from(head.hlc.physical_ms).unwrap_or(i64::MAX),
            i64::from(head.hlc.logical),
            i64::try_from(head.seq).unwrap_or(i64::MAX),
            &head.device[..],
            extra,
        ],
    )?;
    Ok(())
}

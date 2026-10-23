//! The `preferences` row: the Preferences projection's writer.

use crate::engine::lww::LwwStamp;
use rusqlite::{params, Transaction};
use sunrise_domain::Preferences;

fn encode(v: &impl serde::Serialize) -> rusqlite::Result<Vec<u8>> {
    sunrise_cbor::encode_canonical(v)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.to_string().into()))
}

/// Write the merged entity to its row. The row is only ever the merge's
/// projection, so it is written whole.
pub(in crate::engine::merge) fn upsert_preferences_row(
    tx: &Transaction<'_>,
    p: &Preferences,
    head: &LwwStamp,
) -> rusqlite::Result<()> {
    let extra = if p.unknown.is_empty() {
        None
    } else {
        Some(encode(&p.unknown)?)
    };
    tx.execute(
        "INSERT INTO preferences
         (id, values_cbor, created_at_ms, updated_at_ms,
          lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device, extra)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (id) DO UPDATE SET
            values_cbor = excluded.values_cbor,
            updated_at_ms = excluded.updated_at_ms,
            lww_hlc_ms = excluded.lww_hlc_ms,
            lww_hlc_logical = excluded.lww_hlc_logical,
            lww_seq = excluded.lww_seq,
            lww_device = excluded.lww_device,
            extra = excluded.extra",
        params![
            &p.id.bytes()[..],
            encode(&p.values)?,
            p.created_at.as_millisecond(),
            p.updated_at.as_millisecond(),
            i64::try_from(head.hlc.physical_ms).unwrap_or(i64::MAX),
            i64::from(head.hlc.logical),
            i64::try_from(head.seq).unwrap_or(i64::MAX),
            &head.device[..],
            extra,
        ],
    )?;
    Ok(())
}

//! The `places` row: the `Place` projection's writer (issue #1128,
//! ADR-0051).
//!
//! It writes what the merge folded and nothing else. Presence is not a field
//! of the record, so it cannot reach the row (ADR-0051 §3).

use crate::engine::lww::LwwStamp;
use rusqlite::{params, Transaction};
use sunrise_domain::Place;

/// Write the merged entity to its row. The row is only ever the merge's
/// projection, so it is written whole.
pub(in crate::engine::merge) fn upsert_place_row(
    tx: &Transaction<'_>,
    p: &Place,
    head: &LwwStamp,
) -> rusqlite::Result<()> {
    let extra = if p.unknown.is_empty() {
        None
    } else {
        Some(
            sunrise_cbor::encode_canonical(&p.unknown)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.to_string().into()))?,
        )
    };
    tx.execute(
        "INSERT INTO places
         (id, name, lat_e7, lon_e7, radius_m, address_label, deleted, created_at_ms,
          updated_at_ms, lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device, extra)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (id) DO UPDATE SET
            name = excluded.name,
            lat_e7 = excluded.lat_e7,
            lon_e7 = excluded.lon_e7,
            radius_m = excluded.radius_m,
            address_label = excluded.address_label,
            deleted = excluded.deleted,
            updated_at_ms = excluded.updated_at_ms,
            lww_hlc_ms = excluded.lww_hlc_ms,
            lww_hlc_logical = excluded.lww_hlc_logical,
            lww_seq = excluded.lww_seq,
            lww_device = excluded.lww_device,
            extra = excluded.extra",
        params![
            &p.id.bytes()[..],
            p.name,
            p.lat_e7,
            p.lon_e7,
            p.radius_m,
            p.address_label,
            i64::from(p.deleted),
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

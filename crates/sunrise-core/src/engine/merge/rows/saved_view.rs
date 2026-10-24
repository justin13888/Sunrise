//! The `saved_views` row and its search row: the `SavedView` projection's
//! writers (issue #341).

use super::search::{index_saved_view, unindex};
use crate::engine::lww::LwwStamp;
use rusqlite::{params, Transaction};
use sunrise_domain::search::SearchKind;
use sunrise_domain::SavedView;

fn encode(v: &impl serde::Serialize) -> rusqlite::Result<Vec<u8>> {
    sunrise_cbor::encode_canonical(v)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.to_string().into()))
}

/// Write the merged entity to its row, and its name to the search index
/// while it is live. The row is only ever the merge's projection, so it is
/// written whole.
pub(in crate::engine::merge) fn upsert_saved_view_row(
    tx: &Transaction<'_>,
    v: &SavedView,
    head: &LwwStamp,
) -> rusqlite::Result<()> {
    let extra = if v.unknown.is_empty() {
        None
    } else {
        Some(encode(&v.unknown)?)
    };
    tx.execute(
        "INSERT INTO saved_views
         (id, name, view, filter, sort, sort_order, deleted, created_at_ms, updated_at_ms,
          lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device, extra)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (id) DO UPDATE SET
            name = excluded.name,
            view = excluded.view,
            filter = excluded.filter,
            sort = excluded.sort,
            sort_order = excluded.sort_order,
            deleted = excluded.deleted,
            updated_at_ms = excluded.updated_at_ms,
            lww_hlc_ms = excluded.lww_hlc_ms,
            lww_hlc_logical = excluded.lww_hlc_logical,
            lww_seq = excluded.lww_seq,
            lww_device = excluded.lww_device,
            extra = excluded.extra",
        params![
            &v.id.bytes()[..],
            v.name,
            v.view.as_str(),
            encode(&v.filter)?,
            v.sort.as_str(),
            v.sort_order,
            i64::from(v.deleted),
            v.created_at.as_millisecond(),
            v.updated_at.as_millisecond(),
            i64::try_from(head.hlc.physical_ms).unwrap_or(i64::MAX),
            i64::from(head.hlc.logical),
            i64::try_from(head.seq).unwrap_or(i64::MAX),
            &head.device[..],
            extra,
        ],
    )?;
    if v.deleted {
        unindex(tx, SearchKind::View, v.id.bytes())
    } else {
        index_saved_view(tx, v)
    }
}

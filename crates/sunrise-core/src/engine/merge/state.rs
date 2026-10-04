//! The merge state's tables (migration 0033): reading and writing them, one
//! field write at a time.
//!
//! Every write here is idempotent, because the same op can be folded twice:
//! once from its envelope and once more when its row is read back
//! (`sync_from_row`). A register keeps the greater stamp and rewrites itself
//! at an equal one; an OR-set add or remove and a counter delta are keyed by
//! the op that made them.

use super::OpRef;
use super::Stamp;
use crate::engine::ids::blob16;
use crate::engine::lww::{lww_wins, RowLww};
use ciborium::value::Value;
use rusqlite::{params, OptionalExtension, Transaction};
use sunrise_cbor::hlc::Hlc;

/// Whether a write stamped `at` replaces the one a register holds, stamped
/// `held`: the register rule, [`lww_wins`]. An equal stamp is the same op
/// folded again, and replaces itself with the same value.
fn replaces(at: Stamp, held: Stamp) -> bool {
    lww_wins(
        &at.lww(),
        &RowLww {
            hlc: held.hlc,
            device: Some(held.device.to_vec()),
            seq: held.seq,
        },
    )
}

// ---- CBOR helpers ----

pub(super) fn enc(v: &Value) -> rusqlite::Result<Vec<u8>> {
    sunrise_cbor::encode_canonical(v)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

pub(super) fn dec(bytes: &[u8]) -> Value {
    ciborium::de::from_reader(bytes).unwrap_or(Value::Null)
}

// ---- state I/O ----

/// The per-entity merge bookkeeping (`merge_entities`). See migration 0033
/// for what each column means.
#[derive(Debug, Clone)]
pub(super) struct Meta {
    /// A create, or any legacy op, has been applied: the entity is projected.
    pub(super) created: bool,
    /// The least stamp among those ops.
    pub(super) create: Option<Stamp>,
    /// The greatest stamp among the legacy full-state ops applied.
    pub(super) legacy: Option<Stamp>,
    /// The greatest `hlc.physical_ms` among the `Patch` ops applied.
    pub(super) patch_ms: Option<u64>,
    /// The greatest stamp of any op applied: the row's stamp.
    pub(super) head: Stamp,
    /// The row stamp this merge last wrote or folded.
    pub(super) row: Option<(Hlc, [u8; 16], u64)>,
}

impl Meta {
    pub(super) const fn fresh(at: Stamp) -> Self {
        Self {
            created: false,
            create: None,
            legacy: None,
            patch_ms: None,
            head: at,
            row: None,
        }
    }

    pub(super) fn note(&mut self, at: Stamp) {
        self.head = self.head.max(at);
    }

    pub(super) fn note_create(&mut self, at: Stamp) {
        self.created = true;
        self.create = Some(self.create.map_or(at, |c| c.min(at)));
    }
}

pub(super) fn hlc_of(ms: i64, logical: i64) -> Hlc {
    Hlc {
        physical_ms: u64::try_from(ms.max(0)).unwrap_or(0),
        logical: u32::try_from(logical.max(0)).unwrap_or(u32::MAX),
    }
}

pub(super) fn stamp_cols(
    ms: Option<i64>,
    logical: Option<i64>,
    device: Option<Vec<u8>>,
    seq: Option<i64>,
    stream: Option<Vec<u8>>,
) -> Option<Stamp> {
    Some(Stamp {
        hlc: hlc_of(ms?, logical?),
        device: blob16(&device?),
        seq: u64::try_from(seq?.max(0)).unwrap_or(0),
        stream: blob16(&stream?),
    })
}

pub(super) fn read_meta(tx: &Transaction<'_>, id: &[u8; 16]) -> rusqlite::Result<Option<Meta>> {
    tx.query_row(
        "SELECT created,
                create_hlc_ms, create_hlc_logical, create_device, create_seq, create_stream,
                legacy_hlc_ms, legacy_hlc_logical, legacy_device, legacy_seq, legacy_stream,
                patch_ms,
                head_hlc_ms, head_hlc_logical, head_device, head_seq, head_stream,
                row_hlc_ms, row_hlc_logical, row_device, row_seq
         FROM merge_entities WHERE entity_id = ?",
        params![&id[..]],
        |r| {
            let row = match (
                r.get::<_, Option<i64>>(17)?,
                r.get::<_, Option<i64>>(18)?,
                r.get::<_, Option<Vec<u8>>>(19)?,
                r.get::<_, Option<i64>>(20)?,
            ) {
                (Some(ms), Some(logical), Some(device), Some(seq)) => Some((
                    hlc_of(ms, logical),
                    blob16(&device),
                    u64::try_from(seq.max(0)).unwrap_or(0),
                )),
                _ => None,
            };
            Ok(Meta {
                created: r.get::<_, i64>(0)? != 0,
                create: stamp_cols(r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?),
                legacy: stamp_cols(r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?),
                patch_ms: r
                    .get::<_, Option<i64>>(11)?
                    .map(|ms| u64::try_from(ms.max(0)).unwrap_or(0)),
                head: stamp_cols(r.get(12)?, r.get(13)?, r.get(14)?, r.get(15)?, r.get(16)?)
                    .unwrap_or(Stamp {
                        hlc: Hlc::at(0),
                        device: [0u8; 16],
                        seq: 0,
                        stream: [0u8; 16],
                    }),
                row,
            })
        },
    )
    .optional()
}

#[allow(clippy::type_complexity)]
fn split(
    s: Option<Stamp>,
) -> (
    Option<u64>,
    Option<u32>,
    Option<Vec<u8>>,
    Option<u64>,
    Option<Vec<u8>>,
) {
    match s {
        None => (None, None, None, None, None),
        Some(s) => (
            Some(s.hlc.physical_ms),
            Some(s.hlc.logical),
            Some(s.device.to_vec()),
            Some(s.seq),
            Some(s.stream.to_vec()),
        ),
    }
}

pub(super) fn write_meta(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    tag: &str,
    m: &Meta,
) -> rusqlite::Result<()> {
    let c = split(m.create);
    let l = split(m.legacy);
    let (row_ms, row_logical, row_device, row_seq) = match m.row {
        None => (None, None, None, None),
        Some((hlc, device, seq)) => (
            Some(hlc.physical_ms),
            Some(hlc.logical),
            Some(device.to_vec()),
            Some(seq),
        ),
    };
    tx.execute(
        "INSERT INTO merge_entities
         (entity_id, kind, created,
          create_hlc_ms, create_hlc_logical, create_device, create_seq, create_stream,
          legacy_hlc_ms, legacy_hlc_logical, legacy_device, legacy_seq, legacy_stream,
          patch_ms, head_hlc_ms, head_hlc_logical, head_device, head_seq, head_stream,
          row_hlc_ms, row_hlc_logical, row_device, row_seq)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (entity_id) DO UPDATE SET
            created = excluded.created,
            create_hlc_ms = excluded.create_hlc_ms,
            create_hlc_logical = excluded.create_hlc_logical,
            create_device = excluded.create_device,
            create_seq = excluded.create_seq,
            create_stream = excluded.create_stream,
            legacy_hlc_ms = excluded.legacy_hlc_ms,
            legacy_hlc_logical = excluded.legacy_hlc_logical,
            legacy_device = excluded.legacy_device,
            legacy_seq = excluded.legacy_seq,
            legacy_stream = excluded.legacy_stream,
            patch_ms = excluded.patch_ms,
            head_hlc_ms = excluded.head_hlc_ms,
            head_hlc_logical = excluded.head_hlc_logical,
            head_device = excluded.head_device,
            head_seq = excluded.head_seq,
            head_stream = excluded.head_stream,
            row_hlc_ms = excluded.row_hlc_ms,
            row_hlc_logical = excluded.row_hlc_logical,
            row_device = excluded.row_device,
            row_seq = excluded.row_seq",
        params![
            &id[..],
            tag,
            i64::from(m.created),
            c.0,
            c.1,
            c.2,
            c.3,
            c.4,
            l.0,
            l.1,
            l.2,
            l.3,
            l.4,
            m.patch_ms,
            m.head.hlc.physical_ms,
            m.head.hlc.logical,
            &m.head.device[..],
            m.head.seq,
            &m.head.stream[..],
            row_ms,
            row_logical,
            row_device,
            row_seq,
        ],
    )?;
    Ok(())
}

/// Write one register if `at` is not older than what it holds. An equal
/// stamp is the same op folded again, and rewrites the same value.
pub(super) fn write_register(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    field: &str,
    value: Option<&Value>,
    at: Stamp,
    origin: &str,
) -> rusqlite::Result<()> {
    let held = tx
        .query_row(
            "SELECT hlc_ms, hlc_logical, device, seq, stream FROM merge_registers
             WHERE entity_id = ? AND field = ?",
            params![&id[..], field],
            |r| {
                Ok(stamp_cols(
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                ))
            },
        )
        .optional()?
        .flatten();
    if held.is_some_and(|h| !replaces(at, h)) {
        return Ok(());
    }
    let bytes = value.map(enc).transpose()?;
    tx.execute(
        "INSERT INTO merge_registers
         (entity_id, field, value, hlc_ms, hlc_logical, device, seq, stream, origin)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (entity_id, field) DO UPDATE SET
            value = excluded.value, hlc_ms = excluded.hlc_ms,
            hlc_logical = excluded.hlc_logical, device = excluded.device,
            seq = excluded.seq, stream = excluded.stream, origin = excluded.origin",
        params![
            &id[..],
            field,
            bytes,
            at.hlc.physical_ms,
            at.hlc.logical,
            &at.device[..],
            at.seq,
            &at.stream[..],
            origin,
        ],
    )?;
    Ok(())
}

pub(super) fn write_map_entry(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    field: &str,
    key: &str,
    value: &Value,
    at: Stamp,
    origin: &str,
) -> rusqlite::Result<()> {
    let held = tx
        .query_row(
            "SELECT hlc_ms, hlc_logical, device, seq, stream FROM merge_map_entries
             WHERE entity_id = ? AND field = ? AND map_key = ?",
            params![&id[..], field, key],
            |r| {
                Ok(stamp_cols(
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                ))
            },
        )
        .optional()?
        .flatten();
    if held.is_some_and(|h| !replaces(at, h)) {
        return Ok(());
    }
    tx.execute(
        "INSERT INTO merge_map_entries
         (entity_id, field, map_key, value, hlc_ms, hlc_logical, device, seq, stream, origin)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (entity_id, field, map_key) DO UPDATE SET
            value = excluded.value, hlc_ms = excluded.hlc_ms,
            hlc_logical = excluded.hlc_logical, device = excluded.device,
            seq = excluded.seq, stream = excluded.stream, origin = excluded.origin",
        params![
            &id[..],
            field,
            key,
            enc(value)?,
            at.hlc.physical_ms,
            at.hlc.logical,
            &at.device[..],
            at.seq,
            &at.stream[..],
            origin,
        ],
    )?;
    Ok(())
}

pub(super) fn add_element(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    field: &str,
    element: &Value,
    at: Stamp,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO merge_orset_adds
         (entity_id, field, element, tag_stream, tag_device, tag_seq, hlc_ms, hlc_logical)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            &id[..],
            field,
            enc(element)?,
            &at.stream[..],
            &at.device[..],
            at.seq,
            at.hlc.physical_ms,
            at.hlc.logical,
        ],
    )?;
    Ok(())
}

pub(super) fn remove_element(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    field: &str,
    element: &Value,
    tag: OpRef,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO merge_orset_removes
         (entity_id, field, element, tag_stream, tag_device, tag_seq)
         VALUES (?, ?, ?, ?, ?, ?)",
        params![
            &id[..],
            field,
            enc(element)?,
            &tag.stream[..],
            &tag.device[..],
            tag.seq,
        ],
    )?;
    Ok(())
}

pub(super) fn add_delta(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    field: &str,
    delta: i64,
    at: Stamp,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO merge_counter_deltas
         (entity_id, field, op_stream, op_device, op_seq, hlc_ms, hlc_logical, delta)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            &id[..],
            field,
            &at.stream[..],
            &at.device[..],
            at.seq,
            at.hlc.physical_ms,
            at.hlc.logical,
            delta,
        ],
    )?;
    Ok(())
}

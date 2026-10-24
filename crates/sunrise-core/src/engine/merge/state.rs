//! The merge state's tables (migration 0033): reading and writing them, one
//! field write at a time.
//!
//! Every write here is idempotent, because the same op can be folded twice:
//! by a redelivery, and by a snapshot join that carries what an op already
//! folded here wrote. A register keeps the greater stamp and rewrites itself
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
/// `held`: the register rule, [`lww_wins`], over `(hlc, device, seq)`, with
/// the stream breaking a tie between two ops one device made in two streams.
/// So a register keeps the greatest full [`Stamp`], the same order the legacy
/// floor is compared in (`project::superseded`), and a register written at or
/// above the floor is never read as below it. An equal stamp is the same op
/// folded again, and replaces itself with the same value.
fn replaces(at: Stamp, held: Stamp) -> bool {
    if at.lww() == held.lww() {
        return at.stream >= held.stream;
    }
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

/// A stored field value, read under `sunrise_cbor::MAX_DOC_DEPTH`.
///
/// A value an applied op wrote is shallower than the op by the levels the op
/// wraps it in, so the bound refuses none of them. It does hold a value a
/// joined snapshot carried to the same bound an op is held to.
pub(super) fn dec(bytes: &[u8]) -> Value {
    sunrise_cbor::decode_bounded(&mut &bytes[..], sunrise_cbor::MAX_DOC_DEPTH)
        .unwrap_or(Value::Null)
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
}

impl Meta {
    pub(super) const fn fresh(at: Stamp) -> Self {
        Self {
            created: false,
            create: None,
            legacy: None,
            patch_ms: None,
            head: at,
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
                head_hlc_ms, head_hlc_logical, head_device, head_seq, head_stream
         FROM merge_entities WHERE entity_id = ?",
        params![&id[..]],
        |r| {
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
    // `row_*` is no longer written (migration 0039): a row is the fold's
    // output, so there is no other stamp for it to have.
    tx.execute(
        "INSERT INTO merge_entities
         (entity_id, kind, created,
          create_hlc_ms, create_hlc_logical, create_device, create_seq, create_stream,
          legacy_hlc_ms, legacy_hlc_logical, legacy_device, legacy_seq, legacy_stream,
          patch_ms, head_hlc_ms, head_hlc_logical, head_device, head_seq, head_stream)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
            head_stream = excluded.head_stream",
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

/// Add `element` under `at`'s tag, unless `at` is below the legacy floor,
/// where the add would read as removed anyway (see the module docs of
/// [`super`]) and storing it would only grow the table.
pub(super) fn add_element(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    field: &str,
    element: &Value,
    at: Stamp,
    floor: Option<Stamp>,
) -> rusqlite::Result<()> {
    if floor.is_some_and(|l| at < l) {
        return Ok(());
    }
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

/// Record one `inc`, unless `at` is at or below the legacy floor, whose base
/// already counts it.
pub(super) fn add_delta(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    field: &str,
    delta: i64,
    at: Stamp,
    floor: Option<Stamp>,
) -> rusqlite::Result<()> {
    if floor.is_some_and(|l| at <= l) {
        return Ok(());
    }
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

/// Delete the state the legacy floor `floor` has made unreadable: every OR-set
/// add stamped below it, and every counter delta at or below it.
///
/// Each legacy op re-adds every element it carries under its own tag, so
/// without this a long-lived set (`Routine.streak_keys`) written by full-state
/// updates would hold one add per element per op, and every projection would
/// read them all. Nothing deleted here can be read again: the floor only
/// rises, and [`add_element`] and [`add_delta`] refuse a redelivered write
/// below it.
///
/// The removes that named a pruned add are kept. One is inert, since its add
/// never comes back, but deleting it here would make the remove set depend on
/// arrival order: a remove folded before the floor rose would be pruned with
/// its add, and the same remove folded after would be stored, because nothing
/// can tell an add the floor pruned from one that has not arrived yet. Kept,
/// every replica, and every rebuild of one, holds the same removes. Only a
/// `Patch` writes a remove, so the set grows with field ops rather than with
/// full-state ones; bounding it is compaction's (ADR-0065 §8).
pub(super) fn prune_below_floor(
    tx: &Transaction<'_>,
    id: &[u8; 16],
    floor: Stamp,
) -> rusqlite::Result<()> {
    let floor_params = params![
        &id[..],
        floor.hlc.physical_ms,
        floor.hlc.logical,
        &floor.device[..],
        floor.seq,
        &floor.stream[..],
    ];
    tx.execute(
        "DELETE FROM merge_orset_adds
         WHERE entity_id = ?1
           AND (hlc_ms, hlc_logical, tag_device, tag_seq, tag_stream) < (?2, ?3, ?4, ?5, ?6)",
        floor_params,
    )?;
    tx.execute(
        "DELETE FROM merge_counter_deltas
         WHERE entity_id = ?1
           AND (hlc_ms, hlc_logical, op_device, op_seq, op_stream) <= (?2, ?3, ?4, ?5, ?6)",
        floor_params,
    )?;
    Ok(())
}

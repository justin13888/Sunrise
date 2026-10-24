//! The `routines` row: the Routine projection's writers. The local
//! `materialized_until_ms` column is not a projection and is written by the
//! routine engine (`crate::engine::routine`).

use crate::engine::ids::{encode_unknowns, extra_over_opaque, time_to_parts, ExtraTable};
use crate::engine::lww::LwwStamp;
use crate::engine::routine::{
    catchup_policy_str, encode_blob_opt, encode_rrule, encode_streak_state,
};
use crate::engine::task::encode_constraints;
use rusqlite::{params, Transaction};
use sunrise_domain::Routine;

/// The four `SunriseTime` columns of a routine (migration 0041) that the row
/// statements do not write: each one's `_kind`, `_tz` and `_civil`. The
/// index keys are in the row statements.
///
/// `r` is the projected routine read as the engine acts on it
/// ([`Routine::normalize_legacy_times`]), so a legacy bare-instant anchor is
/// stored as the `zoned` value it means.
fn write_time_sidecars(tx: &Transaction<'_>, id: &[u8], r: &Routine) -> rusqlite::Result<()> {
    let (_, s_kind, s_tz, s_civil) = time_to_parts(Some(&r.starts_at));
    let (_, e_kind, e_tz, e_civil) = time_to_parts(r.ends_at.as_ref());
    let (_, p_kind, p_tz, p_civil) = time_to_parts(r.paused_until.as_ref());
    let (_, l_kind, l_tz, l_civil) = time_to_parts(r.last_completed_at.as_ref());
    tx.execute(
        "UPDATE routines SET
            starts_at_kind = ?, starts_at_tz = ?, starts_at_civil = ?,
            ends_at_kind = ?, ends_at_tz = ?, ends_at_civil = ?,
            paused_until_kind = ?, paused_until_tz = ?, paused_until_civil = ?,
            last_completed_at_kind = ?, last_completed_at_tz = ?, last_completed_at_civil = ?
         WHERE id = ?",
        params![
            s_kind, s_tz, s_civil, e_kind, e_tz, e_civil, p_kind, p_tz, p_civil, l_kind, l_tz,
            l_civil, id,
        ],
    )?;
    Ok(())
}

pub(in crate::engine::merge) fn insert_routine_row(
    tx: &Transaction<'_>,
    r: &Routine,
    now_ms: u64,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let r = &r.clone().normalized();
    let id_blob: Vec<u8> = r.id.bytes().to_vec();
    let stream_blob: Vec<u8> = r.template.stream_id.bytes().to_vec();
    let rrule_text = r.rrule.to_rfc5545();
    let rrule_blob = encode_rrule(&r.rrule)?;
    let template_blob = sunrise_cbor::encode_canonical(&r.template)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let skip_dates_blob = encode_blob_opt(&r.skip_dates, r.skip_dates.is_empty())?;
    let skipped_keys_blob = encode_blob_opt(&r.skipped_keys, r.skipped_keys.is_empty())?;
    let constraints_blob = encode_constraints(&r.scheduling_constraints)?;
    let streak_blob = encode_streak_state(r)?;
    let extra_blob = encode_unknowns(&r.unknown)?;
    tx.execute(
        "INSERT INTO routines
         (id, stream_id, rrule_text, rrule_cbor, timezone, starts_at_ms, ends_at_ms,
          streak_counter, paused, archived, deleted, scheduling_constraints,
          template, skip_dates, skipped_keys, catchup_policy,
          last_completed_at_ms, paused_until_ms, created_at_ms, updated_at_ms,
          streak_state, extra, lww_hlc_ms, lww_hlc_logical, lww_seq, lww_device)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            id_blob,
            stream_blob,
            rrule_text,
            rrule_blob,
            r.timezone,
            time_to_parts(Some(&r.starts_at)).0,
            time_to_parts(r.ends_at.as_ref()).0,
            r.streak_counter,
            r.paused as i64,
            r.archived as i64,
            r.deleted as i64,
            constraints_blob,
            template_blob,
            skip_dates_blob,
            skipped_keys_blob,
            catchup_policy_str(&r.catchup_policy),
            time_to_parts(r.last_completed_at.as_ref()).0,
            time_to_parts(r.paused_until.as_ref()).0,
            now_ms,
            now_ms,
            streak_blob,
            extra_blob,
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
        ],
    )?;
    write_time_sidecars(tx, &id_blob, r)
}

pub(in crate::engine::merge) fn update_routine_row(
    tx: &Transaction<'_>,
    r: &Routine,
    lww: &LwwStamp,
) -> rusqlite::Result<()> {
    let r = &r.clone().normalized();
    let id_blob: Vec<u8> = r.id.bytes().to_vec();
    let stream_blob: Vec<u8> = r.template.stream_id.bytes().to_vec();
    let rrule_text = r.rrule.to_rfc5545();
    let rrule_blob = encode_rrule(&r.rrule)?;
    let template_blob = sunrise_cbor::encode_canonical(&r.template)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let skip_dates_blob = encode_blob_opt(&r.skip_dates, r.skip_dates.is_empty())?;
    let skipped_keys_blob = encode_blob_opt(&r.skipped_keys, r.skipped_keys.is_empty())?;
    let constraints_blob = encode_constraints(&r.scheduling_constraints)?;
    let streak_blob = encode_streak_state(r)?;
    let extra_blob = extra_over_opaque(
        tx,
        ExtraTable::Routines,
        &id_blob,
        encode_unknowns(&r.unknown)?,
    )?;
    tx.execute(
        "UPDATE routines SET
            stream_id = ?, rrule_text = ?, rrule_cbor = ?, timezone = ?,
            starts_at_ms = ?, ends_at_ms = ?, streak_counter = ?, paused = ?,
            archived = ?, deleted = ?, scheduling_constraints = ?, template = ?,
            skip_dates = ?, skipped_keys = ?, catchup_policy = ?,
            last_completed_at_ms = ?, paused_until_ms = ?, updated_at_ms = ?,
            streak_state = ?, extra = ?,
            lww_hlc_ms = ?, lww_hlc_logical = ?, lww_seq = ?, lww_device = ?
         WHERE id = ?",
        params![
            stream_blob,
            rrule_text,
            rrule_blob,
            r.timezone,
            time_to_parts(Some(&r.starts_at)).0,
            time_to_parts(r.ends_at.as_ref()).0,
            r.streak_counter,
            r.paused as i64,
            r.archived as i64,
            r.deleted as i64,
            constraints_blob,
            template_blob,
            skip_dates_blob,
            skipped_keys_blob,
            catchup_policy_str(&r.catchup_policy),
            time_to_parts(r.last_completed_at.as_ref()).0,
            time_to_parts(r.paused_until.as_ref()).0,
            r.updated_at.as_millisecond(),
            streak_blob,
            extra_blob,
            lww.hlc.physical_ms,
            lww.hlc.logical,
            lww.seq,
            &lww.device[..],
            id_blob,
        ],
    )?;
    write_time_sidecars(tx, &id_blob, r)
}

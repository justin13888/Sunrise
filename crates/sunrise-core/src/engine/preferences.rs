//! Preferences (ADR-0050, `docs/02-domain/preferences.md`): the commands that
//! write them, the query that resolves them, the row the merge projects the
//! synced entity to, and the plaintext store that holds the keys needed
//! before the vault unlocks.
//!
//! # Two stores in the vault, one outside it
//!
//! - **The entity** is written only by `Patch` ops on the vault-meta stream,
//!   one map register per key (ADR-0044 §4's singleton rule: a device's first
//!   write carries `"create": true` under the fixed `prf_` id). The local write
//!   folds its own op through the same merge a peer's goes through, so this
//!   replica and every other one project the same row.
//! - **The overlay** is `device_preferences`, written directly and never an
//!   op.
//! - **The bootstrap file** is plaintext beside the vault databases and holds
//!   the `bootstrap` keys. It is read and written without the vault, so it is
//!   not reached through a command or a query. [`BootstrapPreferences`] is its
//!   codec; the platform layer does the file I/O.
//!
//! # The gate
//!
//! The entity is the `preferences.entity` feature (ADR-0050 §Consequences).
//! A vault write requires it first ([`super::Engine::require_features`]),
//! which refuses while another device has not advertised support, and the
//! seal guard refuses any `prf_` op sealed before the vault requires it.
//! ADR-0044 §9's `core.field_ops` gate does not apply: it protects entities
//! that a build without per-field merge would overwrite with a full-state op,
//! and no build has a full-state op for this one.

use super::ids::ms_to_ts;
use super::lww::LwwStamp;
use super::merge::merge_op;
use super::{EngineError, META_STREAM};
use crate::commands::CommandResult;
use crate::inner_op::{encode_inner_op, InnerOp, PatchPayload};
use ciborium::value::Value;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::collections::BTreeMap;
use sunrise_domain::preferences::{check_write, is_pref_key};
use sunrise_domain::{
    pref_spec, preferences_ref, resolve_preferences, CborValue, DeviceClass, PrefTarget, PrefValue,
    Preferences, ResolvedPref, Unknowns, PREFERENCE_KEYS,
};
use sunrise_storage::Db;

/// The feature id the entity is gated on.
pub(crate) const PREFERENCES_FEATURE: &str = "preferences.entity";

fn cbor_err(e: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(e.to_string().into())
}

fn encode(v: &impl serde::Serialize) -> rusqlite::Result<Vec<u8>> {
    sunrise_cbor::encode_canonical(v).map_err(cbor_err)
}

/// Write the merged entity to its row. The row is only ever the merge's
/// projection, so it is written whole.
pub(super) fn upsert_preferences_row(
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

/// The projected entity, or `None` before any create has been applied.
pub(crate) fn read_preferences(
    conn: &Connection,
    id: &[u8; 16],
) -> Result<Option<Preferences>, EngineError> {
    /// `values_cbor`, `created_at_ms`, `updated_at_ms`, `extra`.
    type Row = (Vec<u8>, i64, i64, Option<Vec<u8>>);
    let row: Option<Row> = conn
        .query_row(
            "SELECT values_cbor, created_at_ms, updated_at_ms, extra
             FROM preferences WHERE id = ?",
            params![&id[..]],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((values, created, updated, extra)) = row else {
        return Ok(None);
    };
    let values: BTreeMap<String, CborValue> =
        sunrise_cbor::decode_lenient(&values).map_err(|e| EngineError::Cbor(e.to_string()))?;
    Ok(Some(Preferences {
        id: preferences_ref(),
        created_at: ms_to_ts(created),
        updated_at: ms_to_ts(updated),
        values,
        unknown: super::ids::decode_unknowns(extra),
    }))
}

/// This device's overlay, every key it holds.
fn read_overlay(conn: &Connection) -> Result<BTreeMap<String, CborValue>, EngineError> {
    let mut stmt = conn.prepare("SELECT key, value FROM device_preferences")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
    })?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (key, value) = row?;
        // A value that is not CBOR is kept in the table and read as unset.
        if let Ok(v) = sunrise_cbor::decode_lenient::<CborValue>(&value) {
            out.insert(key, v);
        }
    }
    Ok(out)
}

impl super::Engine {
    /// This device's class, by the platform its certificate names.
    fn device_class(&self, conn: &Connection) -> rusqlite::Result<DeviceClass> {
        let platform: Option<String> = conn
            .query_row(
                "SELECT platform FROM devices WHERE device_id = ?",
                params![&self.keychain.device_id()[..]],
                |r| r.get(0),
            )
            .optional()?;
        Ok(DeviceClass::of_platform(
            platform.as_deref().unwrap_or(std::env::consts::OS),
        ))
    }

    /// `Query::Preferences`: every key but the bootstrap ones, resolved.
    pub(super) fn query_preferences(&self, db: &Db) -> Result<Vec<ResolvedPref>, EngineError> {
        let conn = db.conn();
        let vault = read_preferences(conn, preferences_ref().bytes())?
            .map(|p| p.values)
            .unwrap_or_default();
        let overlay = read_overlay(conn)?;
        Ok(resolve_preferences(
            &overlay,
            &vault,
            self.device_class(conn)?,
        ))
    }

    /// `Command::SetPreference` (`Some`) and `Command::ClearPreference`
    /// (`None`).
    pub(super) fn write_preference(
        &self,
        db: &mut Db,
        key: &str,
        value: Option<PrefValue>,
        target: PrefTarget,
    ) -> Result<CommandResult, EngineError> {
        let spec = check_write(key, value.as_ref(), target)?;
        let encoded = value.as_ref().map(PrefValue::encode);
        match target {
            PrefTarget::Device => {
                db.with_tx(|tx| {
                    match &encoded {
                        Some(v) => tx.execute(
                            "INSERT INTO device_preferences (key, value) VALUES (?, ?)
                             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                            params![spec.key, encode(v)?],
                        )?,
                        None => tx.execute(
                            "DELETE FROM device_preferences WHERE key = ?",
                            params![spec.key],
                        )?,
                    };
                    Ok(())
                })?;
                // No op: the overlay never leaves this device.
                Ok(CommandResult::new(preferences_ref(), None, [0u8; 16], 0))
            }
            PrefTarget::Vault => {
                self.require_features(db, &[PREFERENCES_FEATURE], false)?;
                self.patch_preferences(db, spec.key, encoded)
            }
        }
    }

    /// Emit one `Patch` setting (or, with `None`, clearing) `key` in the
    /// vault's entity, and fold it here.
    fn patch_preferences(
        &self,
        db: &mut Db,
        key: &'static str,
        value: Option<CborValue>,
    ) -> Result<CommandResult, EngineError> {
        let target = preferences_ref();
        let now_ms = self.clock.now_ms();
        let op_id = self.fresh_op_id(now_ms);
        let seq = db.with_tx(|tx| -> rusqlite::Result<u64> {
            let created = read_preferences(tx, target.bytes())
                .map_err(|e| cbor_err(e.to_string()))?
                .is_some();
            let entry = Value::Map(vec![(
                Value::Text("set".into()),
                value.map_or(Value::Null, |v| v.0),
            )]);
            let field_op = Value::Map(vec![(
                Value::Text("map".into()),
                Value::Map(vec![(Value::Text(key.into()), entry)]),
            )]);
            let mut fields = Unknowns::new();
            fields.insert("values".into(), CborValue(field_op));
            let op = InnerOp::Patch(Box::new(PatchPayload {
                target,
                create: !created,
                origin: None,
                fields,
                unknown: Unknowns::new(),
            }));
            let inner = encode_inner_op(&op).map_err(cbor_err)?;
            let slot = self.meta_slot(tx, now_ms)?;
            self.ops_insert_at(
                tx,
                &op_id,
                &META_STREAM,
                slot.seq,
                slot.lww.hlc,
                &inner,
                op.inner_kind(),
                op.target_kind(),
                Some(target.bytes()),
                Some(now_ms),
                None,
                now_ms,
                &[],
                slot.epoch,
                &slot.key,
            )?;
            merge_op(tx, &op, &slot.lww, META_STREAM)?;
            Ok(slot.seq)
        })?;
        Ok(CommandResult::new(target, None, op_id, seq))
    }
}

// ---- bootstrap ----

/// The file name of the bootstrap store, in the directory a client keeps its
/// vaults in.
pub const BOOTSTRAP_FILE: &str = "preferences.bootstrap.json";

/// A bootstrap write the key table refuses.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("bootstrap preferences: {0}")]
pub struct BootstrapError(pub String);

/// The `bootstrap` keys (`sync.relay_url`, `auth.oidc_issuer`,
/// `auth.oidc_client_id`): what a device needs before it can find and unlock
/// its vault, so kept in a plaintext JSON object rather than the encrypted
/// overlay. Every value is a string, and none is user content.
///
/// The codec only: it reads and rewrites the file's bytes and never touches
/// the filesystem, which belongs to the platform layer
/// (`docs/01-architecture/shared-core.md` rule 2). A key this build does not
/// know is kept and written back unchanged. A file that is not a JSON object
/// reads as empty and is replaced by the next write: a device pointed at an
/// unreadable relay configuration must stay repairable.
#[derive(Debug, Clone, Copy)]
pub struct BootstrapPreferences;

impl BootstrapPreferences {
    fn parse(file: Option<&[u8]>) -> serde_json::Map<String, serde_json::Value> {
        file.and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or_default()
    }

    /// Every bootstrap key this build knows, resolved against `file` (the
    /// file's bytes, or `None` when there is no file): its value when it is a
    /// string that fits the key's type, the default otherwise.
    #[must_use]
    pub fn resolve(file: Option<&[u8]>) -> Vec<ResolvedPref> {
        let raw = Self::parse(file);
        PREFERENCE_KEYS
            .iter()
            .filter(|s| s.bootstrap)
            .map(|s| {
                let overlay = raw
                    .get(s.key)
                    .and_then(serde_json::Value::as_str)
                    .map(|v| CborValue(Value::Text(v.to_owned())));
                sunrise_domain::resolve_preference(s, overlay.as_ref(), None, DeviceClass::Desktop)
            })
            .collect()
    }

    /// The file's new bytes with `key` set to `value`, or removed with `None`.
    ///
    /// # Errors
    /// A key that is not a bootstrap key, or a value that does not fit it.
    pub fn set(
        file: Option<&[u8]>,
        key: &str,
        value: Option<&str>,
    ) -> Result<Vec<u8>, BootstrapError> {
        let spec = pref_spec(key)
            .filter(|s| s.bootstrap)
            .ok_or_else(|| BootstrapError(format!("`{key}` is not a bootstrap key")))?;
        debug_assert!(is_pref_key(spec.key));
        if let Some(v) = value {
            if !spec.ty.fits(&PrefValue::Text(v.to_owned())) {
                return Err(BootstrapError(format!("the value does not fit `{key}`")));
            }
        }
        let mut raw = Self::parse(file);
        match value {
            Some(v) => {
                raw.insert(spec.key.to_owned(), serde_json::Value::String(v.to_owned()));
            }
            None => {
                raw.remove(spec.key);
            }
        }
        serde_json::to_vec_pretty(&raw).map_err(|e| BootstrapError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunrise_domain::PrefSource;

    fn read(file: Option<&[u8]>, key: &str) -> ResolvedPref {
        BootstrapPreferences::resolve(file)
            .into_iter()
            .find(|r| r.key == key)
            .unwrap()
    }

    #[test]
    fn the_bootstrap_codec_round_trips_and_keeps_what_it_does_not_know() {
        assert_eq!(read(None, "sync.relay_url").value, None, "no file is empty");

        let file = BootstrapPreferences::set(
            Some(br#"{"future.key": 7}"#),
            "sync.relay_url",
            Some("wss://relay.example/sync"),
        )
        .unwrap();
        let r = read(Some(&file), "sync.relay_url");
        assert_eq!(
            r.value,
            Some(PrefValue::Text("wss://relay.example/sync".into()))
        );
        assert_eq!(r.source, PrefSource::Overlay);
        let raw: serde_json::Value = serde_json::from_slice(&file).unwrap();
        assert_eq!(raw["future.key"], 7, "an unknown key survives a write");

        let file = BootstrapPreferences::set(Some(&file), "sync.relay_url", None).unwrap();
        assert_eq!(read(Some(&file), "sync.relay_url").value, None);

        assert!(BootstrapPreferences::set(None, "week_start", Some("MO")).is_err());
        assert!(BootstrapPreferences::set(None, "auth.oidc_issuer", Some("not a url")).is_err());
    }

    #[test]
    fn an_unreadable_file_reads_as_empty_and_is_repaired_by_a_write() {
        let bad: &[u8] = b"not json";
        assert!(BootstrapPreferences::resolve(Some(bad))
            .iter()
            .all(|r| r.value.is_none()));
        let file =
            BootstrapPreferences::set(Some(bad), "auth.oidc_client_id", Some("sunrise")).unwrap();
        assert_eq!(
            read(Some(&file), "auth.oidc_client_id").value,
            Some(PrefValue::Text("sunrise".into()))
        );
    }
}

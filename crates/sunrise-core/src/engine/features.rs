//! The vault's feature state, engine side (ADR-0045 §7–§8, issue #324).
//!
//! Three things happen here:
//!
//! - **The folds.** `VaultRequires` adds to `vault_required_features`, a
//!   grow-only set; `DeviceFeatures` replaces the sender's row in
//!   `device_features` when its stamp is newer. Both are applied by
//!   `apply_control_op` on receipt and written directly by the local emitter.
//! - **The gate.** [`Engine::seal_guard`] runs where every local op is sealed.
//!   It refuses an op whose entity a missing feature's scope covers, and an op
//!   that uses a feature no `VaultRequires` has named yet. The refusal travels
//!   out of the transaction as a boxed `rusqlite` error, which aborts
//!   and rolls it back, and [`EngineError::lift_seal_refusal`] turns it back
//!   into a typed error at [`Engine::apply`].
//! - **Emission.** [`Engine::require_features`] emits `VaultRequires` before a
//!   feature's first op; [`Engine::advertise_features`] emits `DeviceFeatures`
//!   when what this build supports differs from what it last said.

use super::{Engine, EngineError};
use crate::control_op::{DeviceFeaturesPayload, VaultRequiresPayload};
use crate::feature::{self, MissingFeature, SealRefusal};
use crate::inner_op::InnerOp;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use sunrise_cbor::hlc::Hlc;
use sunrise_storage::{Db, DbError};

impl EngineError {
    /// Turn a [`SealRefusal`] that aborted a write transaction back into the
    /// typed error it stands for. Anything else passes through unchanged.
    pub(super) fn lift_seal_refusal(self) -> Self {
        let refusal = match &self {
            Self::Sqlite(e) | Self::Storage(DbError::Sqlite(e)) => seal_refusal(e),
            _ => None,
        };
        match refusal {
            Some(SealRefusal::Missing(feature)) => Self::FeatureMissing { feature },
            Some(other @ SealRefusal::NotRequired(_)) => Self::Invalid(other.to_string()),
            None => self,
        }
    }
}

fn seal_refusal(e: &rusqlite::Error) -> Option<SealRefusal> {
    match e {
        rusqlite::Error::ToSqlConversionFailure(inner) => {
            inner.downcast_ref::<SealRefusal>().cloned()
        }
        _ => None,
    }
}

/// A [`SealRefusal`] as a `rusqlite` error. `ToSqlConversionFailure` is the
/// one variant that boxes an arbitrary error without an optional crate
/// feature. Other engine code raises it too, around encoding failures, but
/// [`seal_refusal`] downcasts to this one type, so it cannot mistake one of
/// those for a refusal.
fn refuse(r: SealRefusal) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(r))
}

/// The vault's required feature ids, sorted.
fn required_ids(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT feature FROM vault_required_features ORDER BY feature")?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    rows.collect()
}

/// `ids` as `device_features.features` stores them: sorted, de-duplicated,
/// joined by newlines. Equal sets are equal text.
fn encode_list<'a>(ids: impl IntoIterator<Item = &'a str>) -> String {
    let mut ids: Vec<&str> = ids.into_iter().collect();
    ids.sort_unstable();
    ids.dedup();
    ids.join("\n")
}

/// Add each well-formed id in `ids` to the vault's required set. Returns how
/// many were new.
fn record_required(tx: &Transaction<'_>, ids: &[String], now_ms: u64) -> rusqlite::Result<usize> {
    let mut added = 0;
    for id in ids {
        if !feature::is_feature_id(id) {
            tracing::warn!(
                ev = "core.feature.id_rejected",
                "a vault_requires op named a malformed feature id; it was skipped"
            );
            continue;
        }
        added += tx.execute(
            "INSERT OR IGNORE INTO vault_required_features (feature, recorded_at_ms) \
             VALUES (?, ?)",
            params![id, now_ms as i64],
        )?;
    }
    Ok(added)
}

/// Replace `device`'s advertised list when `hlc` is newer than the stored
/// one, or equal with a greater list, so every replica keeps the same row
/// whatever order the ops arrive in.
fn record_device_features(
    tx: &Transaction<'_>,
    device: &[u8; 16],
    ids: &[String],
    hlc: Hlc,
    now_ms: u64,
) -> rusqlite::Result<()> {
    let list = encode_list(
        ids.iter()
            .map(String::as_str)
            .filter(|id| feature::is_feature_id(id)),
    );
    tx.execute(
        "INSERT INTO device_features (device_id, features, hlc_ms, hlc_logical, recorded_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT (device_id) DO UPDATE SET \
             features = excluded.features, \
             hlc_ms = excluded.hlc_ms, \
             hlc_logical = excluded.hlc_logical, \
             recorded_at_ms = excluded.recorded_at_ms \
         WHERE (excluded.hlc_ms, excluded.hlc_logical, excluded.features) \
             > (device_features.hlc_ms, device_features.hlc_logical, device_features.features)",
        params![
            &device[..],
            list,
            hlc.physical_ms as i64,
            i64::from(hlc.logical),
            now_ms as i64
        ],
    )?;
    Ok(())
}

impl Engine {
    /// The features the vault requires that this build does not support, each
    /// with what it locks here, sorted by id. Empty is the ordinary case.
    ///
    /// # Errors
    /// Storage failures reading the required set.
    pub fn missing_features(&self, db: &Db) -> Result<Vec<MissingFeature>, EngineError> {
        let required = required_ids(db.conn())?;
        Ok(feature::missing(
            required.iter().map(String::as_str),
            self.features,
        ))
    }

    /// Refuse to seal a local op that a missing feature locks, or that uses a
    /// feature the vault has not been told it requires.
    ///
    /// Called for every local op, entity and control alike, before it is
    /// sealed. A control op's `target_kind` is never refused (see
    /// [`feature::FeatureScope::locks`]), so revocation, rotation and pairing
    /// go on working in a vault this build can only read.
    ///
    /// # Errors
    /// A [`SealRefusal`] boxed in a `rusqlite` error, which aborts `tx`.
    pub(super) fn seal_guard(
        &self,
        tx: &Transaction<'_>,
        inner_op: &[u8],
        target_kind: &str,
    ) -> rusqlite::Result<()> {
        let required = required_ids(tx)?;
        if !required.is_empty() {
            let missing = feature::missing(required.iter().map(String::as_str), self.features);
            if let Some(id) = feature::refusal(&missing, target_kind) {
                return Err(refuse(SealRefusal::Missing(id.to_owned())));
            }
        }
        if let Some(id) = feature::features_used(self.features, inner_op)
            .into_iter()
            .find(|id| !required.iter().any(|r| r == id))
        {
            return Err(refuse(SealRefusal::NotRequired(id.to_owned())));
        }
        Ok(())
    }

    /// Apply a received `VaultRequires`: add its well-formed ids to the
    /// required set. Never refuses; a malformed id is skipped and logged.
    pub(super) fn apply_vault_requires(
        &self,
        tx: &Transaction<'_>,
        p: &VaultRequiresPayload,
        now_ms: u64,
    ) -> rusqlite::Result<()> {
        record_required(tx, &p.features, now_ms).map(|_| ())
    }

    /// Apply a received `DeviceFeatures` from `sender`: keep it when its
    /// stamp is the sender's newest.
    pub(super) fn apply_device_features(
        &self,
        tx: &Transaction<'_>,
        p: &DeviceFeaturesPayload,
        sender: &[u8; 16],
        hlc: Hlc,
        now_ms: u64,
    ) -> rusqlite::Result<()> {
        record_device_features(tx, sender, &p.features, hlc, now_ms)
    }

    /// Make the vault require `ids`, emitting one `VaultRequires` for those it
    /// does not require yet. Returns the ids that were added; empty means the
    /// vault already required them all and nothing was emitted.
    ///
    /// A feature's command path calls this **before** the first op that uses
    /// the feature (ADR-0045 §7). [`Self::seal_guard`] refuses such an op
    /// until it has, so forgetting is a failing test rather than an older
    /// device overwriting data.
    ///
    /// Unless `confirmed`, it refuses while any other non-revoked device has
    /// not said it supports one of the new ids, naming those devices; the
    /// caller asks the user ("Your iPhone needs an update first") and retries
    /// with `confirmed`. A device that has never advertised supports nothing,
    /// which covers every build that predates parking and so cannot keep the
    /// ops it does not understand.
    ///
    /// # Errors
    /// [`EngineError::Invalid`] for an id this build does not support: a
    /// build cannot promise data it cannot write.
    /// [`EngineError::FeatureUnsupportedByDevices`] as above. Storage failures.
    pub fn require_features(
        &self,
        db: &mut Db,
        ids: &[&str],
        confirmed: bool,
    ) -> Result<Vec<String>, EngineError> {
        if let Some(id) = ids
            .iter()
            .find(|id| !self.features.iter().any(|f| f.id == **id))
        {
            return Err(EngineError::Invalid(format!(
                "`{id}` is not a feature this build supports"
            )));
        }
        let required = required_ids(db.conn())?;
        let mut new: Vec<String> = ids
            .iter()
            .filter(|id| !required.iter().any(|r| r == **id))
            .map(|id| (*id).to_owned())
            .collect();
        new.sort_unstable();
        new.dedup();
        if new.is_empty() {
            return Ok(new);
        }
        if !confirmed {
            for id in &new {
                let devices = self.devices_lacking(db.conn(), id)?;
                if !devices.is_empty() {
                    return Err(EngineError::FeatureUnsupportedByDevices {
                        feature: id.clone(),
                        devices,
                    });
                }
            }
        }
        let now_ms = self.clock.now_ms();
        let inner = InnerOp::VaultRequires(VaultRequiresPayload {
            features: new.clone(),
            unknown: sunrise_domain::Unknowns::new(),
        });
        db.with_tx(|tx| {
            self.emit_control_op(tx, &inner, now_ms, None)?;
            record_required(tx, &new, now_ms).map(|_| ())
        })?;
        Ok(new)
    }

    /// Emit `DeviceFeatures` when the list this build supports differs from
    /// the last one this device advertised. Returns whether it emitted.
    ///
    /// A build that supports nothing never emits: never having advertised
    /// already means "supports nothing", and the op's list is non-empty by
    /// its CDDL.
    ///
    /// # Errors
    /// Storage failures.
    pub fn advertise_features(&self, db: &mut Db) -> Result<bool, EngineError> {
        if self.features.is_empty() {
            return Ok(false);
        }
        let device = self.keychain.device_id();
        let mine = encode_list(self.features.iter().map(|f| f.id));
        let stored: Option<String> = db
            .conn()
            .query_row(
                "SELECT features FROM device_features WHERE device_id = ?",
                params![&device[..]],
                |r| r.get(0),
            )
            .optional()?;
        if stored.as_deref() == Some(mine.as_str()) {
            return Ok(false);
        }
        let ids: Vec<String> = self.features.iter().map(|f| f.id.to_owned()).collect();
        let inner = InnerOp::DeviceFeatures(DeviceFeaturesPayload {
            features: ids.clone(),
            unknown: sunrise_domain::Unknowns::new(),
        });
        let now_ms = self.clock.now_ms();
        db.with_tx(|tx| {
            self.emit_control_op(tx, &inner, now_ms, None)?;
            // The stamp `emit_control_op` just took. Recording it lets a peer's
            // copy of this op, delivered back, find the row already current.
            let hlc = self.hlc.peek();
            record_device_features(tx, &device, &ids, hlc, now_ms)
        })?;
        Ok(true)
    }

    /// Every other non-revoked device whose latest `DeviceFeatures` does not
    /// list `id`, including those that never sent one.
    fn devices_lacking(&self, conn: &Connection, id: &str) -> rusqlite::Result<Vec<[u8; 16]>> {
        let me = self.keychain.device_id();
        let mut stmt = conn.prepare(
            "SELECT d.device_id, f.features FROM devices d \
             LEFT JOIN device_features f ON f.device_id = d.device_id \
             WHERE d.device_id != ?1 \
               AND NOT EXISTS (SELECT 1 FROM device_revocations r WHERE r.device_id = d.device_id) \
             ORDER BY d.device_id",
        )?;
        let rows = stmt.query_map(params![&me[..]], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (device, list) = row?;
            let supports = list.is_some_and(|l| l.split('\n').any(|f| f == id));
            if supports {
                continue;
            }
            if let Ok(device) = <[u8; 16]>::try_from(device.as_slice()) {
                out.push(device);
            }
        }
        Ok(out)
    }
}

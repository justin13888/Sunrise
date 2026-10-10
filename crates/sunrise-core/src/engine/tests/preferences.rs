//! Preferences (ADR-0050, issue #337): the vault entity, the device overlay,
//! the resolver as `Query::Preferences` serves it, and what crosses between
//! replicas.

use super::*;
use crate::engine::merge::merge_op;
use crate::engine::preferences::{read_preferences, PREFERENCES_FEATURE};
use crate::feature::{features_used, FEATURES};
use crate::inner_op::PatchPayload;
use ciborium::value::Value;
use sunrise_domain::{
    preferences_ref, CborValue, PrefSource, PrefTarget, PrefValue, ResolvedPref, ValidationError,
    Weekday,
};

const ROOT: [u8; 32] = [0x37; 32];

fn clock() -> Arc<FakeClock> {
    Arc::new(FakeClock(PLMutex::new(T0)))
}

fn prefs(e: &Engine, db: &Db) -> Vec<ResolvedPref> {
    match e.query(db, Query::Preferences).unwrap() {
        QueryResult::Preferences(p) => p,
        other => panic!("unexpected {other:?}"),
    }
}

fn pref(e: &Engine, db: &Db, key: &str) -> ResolvedPref {
    prefs(e, db)
        .into_iter()
        .find(|r| r.key == key)
        .unwrap_or_else(|| panic!("{key} is not returned"))
}

fn set(e: &Engine, db: &mut Db, key: &str, value: PrefValue, target: PrefTarget) {
    e.apply(
        db,
        Command::SetPreference {
            key: key.into(),
            value,
            target,
        },
    )
    .unwrap();
}

fn stored(db: &Db) -> std::collections::BTreeMap<String, CborValue> {
    read_preferences(db.conn(), preferences_ref().bytes())
        .unwrap()
        .map(|p| p.values)
        .unwrap_or_default()
}

/// Every Preferences patch `e`'s device sealed into `db`'s log, in seq order.
fn patches_of(e: &Engine, db: &Db) -> Vec<PatchPayload> {
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT envelope FROM ops WHERE inner_kind = 'preferences.patch' AND device_id = ?
             ORDER BY seq",
        )
        .unwrap();
    let envs: Vec<Vec<u8>> = stmt
        .query_map(params![&e.keychain.device_id()[..]], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    envs.iter()
        .map(|env| {
            let cbor = e.keychain.open_op(env).unwrap();
            match decode_inner_op(&cbor).unwrap() {
                InnerOp::Patch(p) => *p,
                other => panic!("not a patch: {other:?}"),
            }
        })
        .collect()
}

/// Two trusting replicas of one vault, each having advertised what it
/// supports to the other, as a fresh `Core::open` does.
struct Pair {
    ea: Engine,
    eb: Engine,
    ca: Arc<FakeClock>,
    cb: Arc<FakeClock>,
    dba: Db,
    dbb: Db,
}

fn pair() -> Pair {
    let (ca, cb) = (clock(), clock());
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);
    assert!(ea.advertise_features(&mut dba).unwrap());
    assert!(eb.advertise_features(&mut dbb).unwrap());
    let mut p = Pair {
        ea,
        eb,
        ca,
        cb,
        dba,
        dbb,
    };
    sync(&mut p);
    p
}

/// Every op one replica logged under its own device id that the other has
/// not applied, delivered in seq order. Both directions.
fn sync(p: &mut Pair) {
    deliver(&p.ea, &p.dba, &p.eb, &mut p.dbb);
    deliver(&p.eb, &p.dbb, &p.ea, &mut p.dba);
}

fn deliver(from: &Engine, from_db: &Db, to: &Engine, to_db: &mut Db) {
    let device = from.keychain.device_id();
    let envs: Vec<Vec<u8>> = {
        let mut stmt = from_db
            .conn()
            .prepare("SELECT envelope FROM ops WHERE device_id = ? ORDER BY stream_id, seq")
            .unwrap();
        stmt.query_map(params![&device[..]], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    for env in envs {
        to.apply_remote(to_db, &env).unwrap();
    }
}

#[test]
fn a_fresh_vault_resolves_every_key_to_its_default() {
    let (e, db) = (engine(), db());
    let all = prefs(&e, &db);
    assert!(all.iter().all(|r| r.source == PrefSource::Default));
    assert!(
        all.iter()
            .all(|r| !r.key.starts_with("sync.") && !r.key.starts_with("auth.")),
        "bootstrap keys are served by the bootstrap store, not the query"
    );
    assert_eq!(
        pref(&e, &db, "week_start").value,
        Some(PrefValue::Weekday(Weekday::Su))
    );
    assert_eq!(
        pref(&e, &db, "notifications.timezone_changed.enabled").value,
        Some(PrefValue::Bool(false))
    );
}

#[test]
fn a_device_write_goes_to_the_overlay_and_emits_no_op() {
    let (e, mut db) = (engine(), db());
    let before = op_count(&db);
    set(
        &e,
        &mut db,
        "keyboard.vim_mode",
        PrefValue::Bool(true),
        PrefTarget::Device,
    );
    assert_eq!(op_count(&db), before, "the overlay is never an op");
    let r = pref(&e, &db, "keyboard.vim_mode");
    assert_eq!(
        (r.value, r.source),
        (Some(PrefValue::Bool(true)), PrefSource::Overlay)
    );

    e.apply(
        &mut db,
        Command::ClearPreference {
            key: "keyboard.vim_mode".into(),
            target: PrefTarget::Device,
        },
    )
    .unwrap();
    assert_eq!(
        pref(&e, &db, "keyboard.vim_mode").source,
        PrefSource::Default
    );
}

#[test]
fn a_target_the_scope_does_not_permit_is_refused_and_writes_nothing() {
    let (e, mut db) = (engine(), db());
    let before = op_count(&db);
    let err = e
        .apply(
            &mut db,
            Command::SetPreference {
                key: "week_start".into(),
                value: PrefValue::Weekday(Weekday::Mo),
                target: PrefTarget::Device,
            },
        )
        .unwrap_err();
    assert!(
        matches!(
            err,
            EngineError::Validation(ValidationError::PreferenceScope)
        ),
        "{err:?}"
    );
    assert_eq!(
        ValidationError::PreferenceScope.as_error_code().as_str(),
        "VALIDATION_PREFERENCE_SCOPE"
    );
    let err = e
        .apply(
            &mut db,
            Command::SetPreference {
                key: "keyboard.vim_mode".into(),
                value: PrefValue::Bool(true),
                target: PrefTarget::Vault,
            },
        )
        .unwrap_err();
    assert!(matches!(
        err,
        EngineError::Validation(ValidationError::PreferenceScope)
    ));
    let err = e
        .apply(
            &mut db,
            Command::SetPreference {
                key: "stale_after_days".into(),
                value: PrefValue::Uint(366),
                target: PrefTarget::Vault,
            },
        )
        .unwrap_err();
    assert!(matches!(
        err,
        EngineError::Validation(ValidationError::Field { .. })
    ));
    assert_eq!(op_count(&db), before);
    assert!(stored(&db).is_empty());
}

/// The first vault write requires the feature, then creates the singleton
/// idempotently; every later one is a plain patch of one key.
#[test]
fn the_first_vault_write_requires_the_feature_and_creates_the_singleton() {
    let (e, mut db) = (engine(), db());
    set(
        &e,
        &mut db,
        "week_start",
        PrefValue::Weekday(Weekday::Mo),
        PrefTarget::Vault,
    );
    set(
        &e,
        &mut db,
        "stale_after_days",
        PrefValue::Uint(7),
        PrefTarget::Vault,
    );

    let required: Vec<String> = db
        .conn()
        .prepare("SELECT feature FROM vault_required_features")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(required, [PREFERENCES_FEATURE]);

    let ops = patches_of(&e, &db);
    assert_eq!(ops.len(), 2);
    assert!(ops[0].create, "the first write creates");
    assert!(!ops[1].create, "a later one does not");
    assert!(ops.iter().all(|p| p.target == preferences_ref()));
    assert_eq!(ops[1].fields.len(), 1, "one field: `values`");

    let r = pref(&e, &db, "week_start");
    assert_eq!(
        (r.value, r.source, r.vault_set),
        (
            Some(PrefValue::Weekday(Weekday::Mo)),
            PrefSource::Vault,
            true
        )
    );
    assert_eq!(
        pref(&e, &db, "stale_after_days").value,
        Some(PrefValue::Uint(7))
    );
}

#[test]
fn an_overridable_key_reads_the_overlay_over_the_vault_until_cleared() {
    let (e, mut db) = (engine(), db());
    let window = |s: i8, en: i8| PrefValue::TimeWindow {
        start: jiff::civil::time(s, 0, 0, 0),
        end: jiff::civil::time(en, 0, 0, 0),
    };
    let key = "notifications.quiet_hours.window";
    set(&e, &mut db, key, window(22, 7), PrefTarget::Vault);
    set(&e, &mut db, key, window(20, 9), PrefTarget::Device);
    let r = pref(&e, &db, key);
    assert_eq!(
        (r.value, r.source),
        (Some(window(20, 9)), PrefSource::Overlay)
    );

    e.apply(
        &mut db,
        Command::ClearPreference {
            key: key.into(),
            target: PrefTarget::Device,
        },
    )
    .unwrap();
    let r = pref(&e, &db, key);
    assert_eq!(
        (r.value, r.source),
        (Some(window(22, 7)), PrefSource::Vault)
    );

    e.apply(
        &mut db,
        Command::ClearPreference {
            key: key.into(),
            target: PrefTarget::Vault,
        },
    )
    .unwrap();
    let r = pref(&e, &db, key);
    assert_eq!((r.value, r.source), (None, PrefSource::Default));
    assert!(!stored(&db).contains_key(key), "a clear tombstones the key");
}

/// The issue's convergence test: two devices each write a different key,
/// concurrently and before either has seen the other's create. Both edits
/// survive on both replicas, and the two creates merge into one entity.
#[test]
fn two_devices_editing_different_keys_both_keep_their_edit() {
    let mut p = pair();
    set_clock(&p.ca, T0 + 1_000);
    set_clock(&p.cb, T0 + 1_000);
    set(
        &p.ea,
        &mut p.dba,
        "week_start",
        PrefValue::Weekday(Weekday::Mo),
        PrefTarget::Vault,
    );
    set(
        &p.eb,
        &mut p.dbb,
        "stale_after_days",
        PrefValue::Uint(3),
        PrefTarget::Vault,
    );
    assert!(patches_of(&p.ea, &p.dba)[0].create && patches_of(&p.eb, &p.dbb)[0].create);

    sync(&mut p);

    for (e, db) in [(&p.ea, &p.dba), (&p.eb, &p.dbb)] {
        assert_eq!(
            pref(e, db, "week_start").value,
            Some(PrefValue::Weekday(Weekday::Mo))
        );
        assert_eq!(
            pref(e, db, "stale_after_days").value,
            Some(PrefValue::Uint(3))
        );
    }
    assert_eq!(stored(&p.dba), stored(&p.dbb));
}

/// Two devices writing the same key converge on one value: the register's
/// last writer by `(hlc, device, seq)`.
#[test]
fn two_devices_writing_one_key_converge_on_the_later_write() {
    let mut p = pair();
    set_clock(&p.ca, T0 + 1_000);
    set_clock(&p.cb, T0 + 2_000);
    set(
        &p.ea,
        &mut p.dba,
        "planner.snap_s",
        PrefValue::Uint(300),
        PrefTarget::Vault,
    );
    set(
        &p.eb,
        &mut p.dbb,
        "planner.snap_s",
        PrefValue::Uint(600),
        PrefTarget::Vault,
    );
    sync(&mut p);
    for (e, db) in [(&p.ea, &p.dba), (&p.eb, &p.dbb)] {
        assert_eq!(
            pref(e, db, "planner.snap_s").value,
            Some(PrefValue::Uint(600))
        );
    }
}

/// A device's overlay is its own: nothing it writes there reaches the other.
#[test]
fn the_overlay_never_crosses_to_another_device() {
    let mut p = pair();
    set(
        &p.ea,
        &mut p.dba,
        "time_format",
        PrefValue::Text("h24".into()),
        PrefTarget::Device,
    );
    set(
        &p.ea,
        &mut p.dba,
        "time_format",
        PrefValue::Text("h12".into()),
        PrefTarget::Vault,
    );
    sync(&mut p);
    assert_eq!(
        pref(&p.ea, &p.dba, "time_format").value,
        Some(PrefValue::Text("h24".into()))
    );
    let b = pref(&p.eb, &p.dbb, "time_format");
    assert_eq!(
        (b.value, b.source),
        (Some(PrefValue::Text("h12".into())), PrefSource::Vault)
    );
}

/// Seal a `Patch` of the Preferences entity the way a newer build would,
/// carrying `entries` in its `values` map, and fold it into the writer.
fn emit_values(e: &Engine, db: &mut Db, entries: Vec<(&str, Value)>) {
    let set = |v: Value| Value::Map(vec![(Value::Text("set".into()), v)]);
    let map = Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (Value::Text(k.into()), set(v)))
            .collect(),
    );
    let mut fields = Unknowns::new();
    fields.insert(
        "values".into(),
        CborValue(Value::Map(vec![(Value::Text("map".into()), map)])),
    );
    let op = InnerOp::Patch(Box::new(PatchPayload {
        target: preferences_ref(),
        create: read_preferences(db.conn(), preferences_ref().bytes())
            .unwrap()
            .is_none(),
        origin: None,
        fields,
        unknown: Unknowns::new(),
    }));
    e.require_features(db, &[PREFERENCES_FEATURE], false)
        .unwrap();
    let bytes = encode_inner_op(&op).unwrap();
    let now = T0 + 5_000;
    db.with_tx(|tx| {
        let slot = e.meta_slot(tx, now)?;
        let op_id = e.fresh_op_id(now);
        e.ops_insert_at(
            tx,
            &op_id,
            &META_STREAM,
            slot.seq,
            slot.lww.hlc,
            &bytes,
            op.inner_kind(),
            op.target_kind(),
            Some(preferences_ref().bytes()),
            Some(now),
            None,
            now,
            &[],
            slot.epoch,
            &slot.key,
        )?;
        merge_op(tx, &op, &slot.lww, META_STREAM)
    })
    .unwrap();
}

/// The lossless rule (ADR-0050 §1), with this build as the older peer: a key
/// it does not know, and a value of a key it knows that no longer fits, both
/// arrive from a newer build. Both are kept byte for byte, neither is
/// resolved, and this build's own later write to another key leaves them as
/// they were on every replica.
#[test]
fn an_unknown_key_and_an_unfit_value_round_trip_through_an_older_peer() {
    let mut p = pair();
    // A plays the newer build.
    emit_values(
        &p.ea,
        &mut p.dba,
        vec![
            ("future.key", Value::Integer(42.into())),
            ("stale_after_days", Value::Integer(400.into())),
        ],
    );
    sync(&mut p);

    // B is the older build: it keeps both and resolves neither.
    let kept = stored(&p.dbb);
    assert_eq!(
        kept.get("future.key"),
        Some(&CborValue(Value::Integer(42.into())))
    );
    assert_eq!(
        kept.get("stale_after_days"),
        Some(&CborValue(Value::Integer(400.into())))
    );
    assert!(prefs(&p.eb, &p.dbb).iter().all(|r| r.key != "future.key"));
    let stale = pref(&p.eb, &p.dbb, "stale_after_days");
    assert_eq!(
        (stale.value, stale.source, stale.vault_set),
        (Some(PrefValue::Uint(14)), PrefSource::Default, true)
    );

    // B writes a key it knows; the two it does not read are untouched.
    set_clock(&p.cb, T0 + 9_000);
    set(
        &p.eb,
        &mut p.dbb,
        "week_start",
        PrefValue::Weekday(Weekday::We),
        PrefTarget::Vault,
    );
    let own = patches_of(&p.eb, &p.dbb)
        .pop()
        .expect("B's write is the newest");
    let values = own.fields.get("values").unwrap().get();
    let written = values.as_map().unwrap()[0].1.as_map().unwrap();
    assert_eq!(
        written
            .iter()
            .map(|(k, _)| k.as_text().unwrap())
            .collect::<Vec<_>>(),
        ["week_start"],
        "B's patch writes only the key it set"
    );
    sync(&mut p);
    for db in [&p.dba, &p.dbb] {
        let v = stored(db);
        assert_eq!(
            v.get("future.key"),
            Some(&CborValue(Value::Integer(42.into())))
        );
        assert_eq!(
            v.get("stale_after_days"),
            Some(&CborValue(Value::Integer(400.into())))
        );
        assert_eq!(
            v.get("week_start"),
            Some(&CborValue(Value::Text("WE".into())))
        );
    }
}

/// Every op on the entity uses `preferences.entity`, which is what makes the
/// seal guard refuse one sealed before the vault requires the feature.
#[test]
fn a_preferences_patch_uses_the_entity_feature() {
    let op = InnerOp::Patch(Box::new(PatchPayload {
        target: preferences_ref(),
        create: true,
        origin: None,
        fields: Unknowns::new(),
        unknown: Unknowns::new(),
    }));
    let bytes = encode_inner_op(&op).unwrap();
    assert_eq!(features_used(FEATURES, &bytes), [PREFERENCES_FEATURE]);
    let task = InnerOp::Patch(Box::new(PatchPayload {
        target: EntityRef::new(EntityKind::Task, [1; 16]),
        create: true,
        origin: None,
        fields: Unknowns::new(),
        unknown: Unknowns::new(),
    }));
    assert!(features_used(FEATURES, &encode_inner_op(&task).unwrap()).is_empty());
}

/// A vault write waits for every other device to support the entity: a
/// device that has not advertised it would refuse the op (ADR-0045 §8).
#[test]
fn a_vault_write_is_refused_while_another_device_lacks_the_feature() {
    let (ea, eb) = (
        engine_seeded(ROOT, [1u8; 32], clock()),
        engine_seeded(ROOT, [2u8; 32], clock()),
    );
    let mut dba = db_root(ROOT);
    trust(&ea, &mut dba, &eb);
    let err = ea
        .apply(
            &mut dba,
            Command::SetPreference {
                key: "week_start".into(),
                value: PrefValue::Weekday(Weekday::Mo),
                target: PrefTarget::Vault,
            },
        )
        .unwrap_err();
    assert!(
        matches!(err, EngineError::FeatureUnsupportedByDevices { .. }),
        "{err:?}"
    );
    assert!(patches_of(&ea, &dba).is_empty());
    // The overlay is this device's alone, and needs nobody's support.
    ea.apply(
        &mut dba,
        Command::SetPreference {
            key: "keyboard.vim_mode".into(),
            value: PrefValue::Bool(true),
            target: PrefTarget::Device,
        },
    )
    .unwrap();
}

//! An inner op nested past `sunrise_cbor::MAX_DOC_DEPTH` is parked, not
//! refused (ADR-0045 §4), and one at the bound applies.
//!
//! The bound stops a peer from making this build recurse 256 levels into an
//! op, but an op that verified and opened is a member's, and a newer build's
//! fields may nest deeper than this build reads. Refusing it would lose it
//! for good once the relay evicts the frame, so it is kept like an op of an
//! unknown kind and retried when the build changes.
//!
//! The snapshot record, read before anything about it is verified, is held to
//! the tighter framing bound instead.

use super::support::*;
use super::*;
use ciborium::value::Value;
use sunrise_cbor::MAX_DOC_DEPTH;

/// `n` one-element arrays around the integer 1.
fn nest(n: usize) -> Value {
    let mut v = Value::Integer(1.into());
    for _ in 0..n {
        v = Value::Array(vec![v]);
    }
    v
}

/// A's create of `task`, re-tagged `TaskUpdate` and carrying one field no
/// build knows, `"zz_newer"`, nested so the whole op is `depth` levels deep:
/// the op's map and the task's map are two of them.
fn deep_update(ea: &Engine, dba: &Db, task: EntityRef, depth: usize) -> Vec<u8> {
    let inner = ea
        .keychain
        .open_op(&create_env_for(dba, task.bytes()))
        .unwrap();
    let Value::Map(mut op) = ciborium::de::from_reader::<Value, _>(inner.as_slice()).unwrap()
    else {
        panic!("an inner op is a map");
    };
    let (_, Value::Map(mut fields)) = op.pop().unwrap() else {
        panic!("a task is a map");
    };
    fields.push((Value::Text("zz_newer".into()), nest(depth - 2)));
    let mut out = Vec::new();
    ciborium::ser::into_writer(
        &Value::Map(vec![(Value::Text("TaskUpdate".into()), Value::Map(fields))]),
        &mut out,
    )
    .unwrap();
    out
}

/// One past the bound: kept in `ops` unapplied, marked `too_deep` under the
/// variant's name, and counted by the cursor, so the relay stops re-sending
/// it and the ops above it are not held behind it. A re-delivery and a replay
/// by the same build change nothing, and the reason stays `too_deep`.
#[test]
fn an_op_nested_past_the_bound_is_parked_and_counted() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);
    let a_id = ea.keychain.device_id();

    let task = new_task(&ea, &mut dba, "t");
    let first = create_env_for(&dba, task.bytes());
    let stream = decode_envelope(&first).unwrap().stream_id;
    assert!(eb.apply_remote(&mut dbb, &first).unwrap().is_some());

    let inner = deep_update(&ea, &dba, task, MAX_DOC_DEPTH + 1);
    let deep = super::emit_raw_inner(&ea, &mut dba, &stream, &inner);
    let deep_id = remote_op_id(&stream, &a_id, decode_envelope(&deep).unwrap().seq);
    assert!(
        eb.apply_remote_all(&mut dbb, &deep).unwrap().is_empty(),
        "parked, not refused"
    );
    assert_eq!(cursor_for(&dbb, &stream, &a_id), 2, "the parked op counts");
    assert_eq!(super::op_row(&dbb, &deep_id), ("unknown".into(), None));
    let want = vec![(
        "too_deep".to_string(),
        "TaskUpdate".to_string(),
        i64::from(sunrise_cbor::version::DOC_SCHEMA_V),
    )];
    assert_eq!(super::parked_rows(&dbb), want);

    let ops_before = op_count(&dbb);
    assert!(eb.apply_remote_all(&mut dbb, &deep).unwrap().is_empty());
    assert!(eb.replay_parked_ops(&mut dbb).is_empty());
    assert_eq!(op_count(&dbb), ops_before);
    assert_eq!(super::parked_rows(&dbb), want);
}

/// At the bound the op applies like any other, and parks nothing.
#[test]
fn an_op_at_the_bound_applies() {
    let ea = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let eb = engine_seeded(ROOT, [2u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);

    let task = new_task(&ea, &mut dba, "t");
    let first = create_env_for(&dba, task.bytes());
    let stream = decode_envelope(&first).unwrap().stream_id;
    assert!(eb.apply_remote(&mut dbb, &first).unwrap().is_some());

    let inner = deep_update(&ea, &dba, task, MAX_DOC_DEPTH);
    let at = super::emit_raw_inner(&ea, &mut dba, &stream, &inner);
    eb.apply_remote_all(&mut dbb, &at)
        .expect("an op at the bound applies");
    assert!(super::parked_rows(&dbb).is_empty(), "nothing parked");
    assert_eq!(cursor_for(&dbb, &stream, &ea.keychain.device_id()), 2);
}

/// A snapshot record is read before its signature is checked, so under the
/// framing bound, `sunrise_cbor::MAX_VALUE_DEPTH`: a record nested to it is
/// read (and refused for its shape), and one level more is not read at all.
#[test]
fn a_snapshot_record_is_read_under_the_framing_bound() {
    use sunrise_cbor::magic::{write_prefix, MagicKind, MAGIC_LEN};
    let e = engine_seeded(ROOT, [1u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let mut db = db_root(ROOT);
    let record = |levels: usize| {
        let mut out = vec![0u8; MAGIC_LEN];
        write_prefix(
            &mut out,
            MagicKind::Snapshot,
            crate::engine::SNAPSHOT_FORMAT_V,
        );
        ciborium::ser::into_writer(&nest(levels), &mut out).unwrap();
        out
    };
    let bound = sunrise_cbor::MAX_VALUE_DEPTH;
    let mut refusal = |levels| match e.apply_snapshot(&mut db, &record(levels)) {
        Err(EngineError::Invalid(why)) => why,
        other => panic!("a nest is no snapshot: {other:?}"),
    };
    assert_eq!(refusal(bound), "snapshot: not a map");
    assert_eq!(refusal(bound + 1), "snapshot: not canonical CBOR");
}

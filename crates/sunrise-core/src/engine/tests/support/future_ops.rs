//! What a newer build writes, as this one receives it: ops whose payload or
//! values it does not know, and the rows that show what it did with them.

use super::*;

/// Build the `unknown` map a newer schema would have written.
pub(in crate::engine::tests) fn future_fields() -> sunrise_domain::Unknowns {
    use ciborium::value::Value;
    let mut u = sunrise_domain::Unknowns::new();
    u.insert(
        "delegated_to".into(),
        sunrise_domain::CborValue(Value::Text("prs_future".into())),
    );
    u.insert(
        "zzz_sort_last".into(),
        sunrise_domain::CborValue(Value::Integer(7.into())),
    );
    u
}

/// Emit, as `sender`, a genuine sealed and signed op on `stream` whose inner
/// CBOR is `inner`, and return its envelope. This is how a newer build's op
/// family looks to this one: every envelope check passes, and only the
/// payload is foreign.
pub(in crate::engine::tests) fn emit_raw_inner(
    sender: &Engine,
    db: &mut Db,
    stream: &[u8; 16],
    inner: &[u8],
) -> Vec<u8> {
    let device = sender.keychain.device_id();
    // Sealed as the newer build that wrote it would have, whatever this
    // vault requires: what is under test is the receiver.
    let op_id = crate::engine::features::sealing_as_peer(|| {
        db.with_tx(|tx| {
            let seq = sender.next_seq_tx(tx, stream)?;
            let op_id = remote_op_id(stream, &device, seq);
            sender.ops_insert(
                tx,
                &op_id,
                stream,
                seq,
                sender.hlc.send(),
                inner,
                "future.kind",
                "future",
                None,
                Some(T0),
                None,
                T0,
                &[],
            )?;
            Ok(op_id)
        })
    })
    .unwrap();
    env_bytes(db, &op_id)
}

pub(in crate::engine::tests) fn future_kind_inner(name: &str) -> Vec<u8> {
    use ciborium::value::Value;
    let mut buf = Vec::new();
    ciborium::ser::into_writer(
        &Value::Map(vec![(
            Value::Text(name.into()),
            Value::Map(vec![(Value::Text("title".into()), Value::Text("x".into()))]),
        )]),
        &mut buf,
    )
    .unwrap();
    buf
}

/// `(reason, kind, parked_under_doc_schema_v)` of every parked row.
pub(in crate::engine::tests) fn parked_rows(db: &Db) -> Vec<(String, String, i64)> {
    let mut stmt = db
        .conn()
        .prepare("SELECT reason, kind, parked_under_doc_schema_v FROM parked_ops")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// `(inner_kind, applied_at)` of the `ops` row for `op_id`.
pub(in crate::engine::tests) fn op_row(db: &Db, op_id: &[u8; 16]) -> (String, Option<i64>) {
    db.conn()
        .query_row(
            "SELECT inner_kind, applied_at FROM ops WHERE op_id = ?",
            params![&op_id[..]],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

/// Set a task's state to one this build does not know.
pub(in crate::engine::tests) fn set_unknown_state(
    e: &Engine,
    db: &mut Db,
    task: EntityRef,
    raw: &str,
) {
    e.apply(
        db,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                state: Some(TaskState::from_raw(raw)),
                ..Default::default()
            },
        },
    )
    .unwrap();
}

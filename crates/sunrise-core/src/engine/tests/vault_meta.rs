//! The vault-meta stream: its keys, and what no command may aim at it.

use super::*;

/// #214: **every** command that writes into the vault-meta log must read its
/// sequence number inside the transaction that may mint that stream's key.
///
/// The mechanism, once, because it is the same for all of them. `ops_insert`
/// resolves the routing stream's epoch, and resolving it can mint the stream's
/// first key; minting emits a `key_envelope` op per recipient, and
/// `emit_control_op` puts every one of those into **vault-meta**, whatever
/// stream was being minted for. So a command whose own op is *also* routed to
/// vault-meta hands out a sequence number the mint then spends.
/// `UNIQUE(stream_id, device_id, seq)` and `INSERT OR IGNORE` do the rest: the
/// losing op disappears and its outbox row fails its foreign key.
///
/// That is why this covers exactly the commands routed to vault-meta and not
/// the ones routed to a Task's own Stream. A `CreateTask` into the Inbox reads
/// the *Inbox's* `seq`, and the envelopes its mint emits land in vault-meta —
/// a different stream, a different counter, no collision. Every command now
/// takes its `seq` inside its transaction, through `Engine::log_slot`.
///
/// `60ee61d` fixed this for `emit_control_op` and `#214` for `create_stream`;
/// the other ten are fixed together with them, and `Engine::log_slot` is now
/// the only way a command obtains a sequence number, so the order cannot be
/// got wrong again.
///
/// Every case runs on its own engine that has never seen `ensure_base_epochs`,
/// because the *first* op of any kind mints vault-meta and there is therefore
/// only one command per vault that can hit this.
#[test]
fn every_vault_meta_command_sequences_after_the_key_it_mints() {
    type Build = fn(&mut Db) -> Command;
    let cases: &[(&str, Build)] = &[
        ("stream.create", |_db| {
            Command::CreateStream(StreamDraft {
                name: "Work".into(),
                ..Default::default()
            })
        }),
        ("stream.update", |db| Command::UpdateStream {
            id: seed_stream(db, "Work"),
            patch: StreamPatch {
                name: Some("Renamed".into()),
                ..Default::default()
            },
        }),
        ("stream.delete", |db| Command::DeleteStream {
            id: seed_stream(db, "Work"),
            disposition: sunrise_domain::Disposition::Detach,
        }),
        ("context.create", |_db| {
            Command::CreateContext(ContextDraft {
                name: "errands".into(),
                ..Default::default()
            })
        }),
        ("context.update", |db| Command::UpdateContext {
            id: seed_context(db, "errands"),
            patch: ContextPatch {
                name: Some("renamed".into()),
                ..Default::default()
            },
        }),
        ("context.delete", |db| {
            Command::DeleteContext(seed_context(db, "errands"))
        }),
        ("routine.create", |_db| {
            Command::CreateRoutine(routine_draft(
                inbox_stream_ref(),
                "FREQ=DAILY",
                T0 as i64,
                RoutineCatchupPolicy::Skip,
                Vec::new(),
            ))
        }),
        ("routine.update", |db| Command::UpdateRoutine {
            scope: sunrise_domain::EditScope::All,
            id: seed_bare_routine(db),
            patch: RoutinePatch {
                grace_window_s: Some(Some(600)),
                ..Default::default()
            },
        }),
        ("routine.delete", |db| {
            Command::DeleteRoutine(seed_bare_routine(db))
        }),
        ("routine.skip", |db| Command::SkipRoutineOccurrence {
            id: seed_bare_routine(db),
            occurrence_key: occurrence_key_at("UTC", ms_to_ts(T0 as i64)).unwrap(),
        }),
        ("review.snapshot", |_db| {
            Command::SaveReviewSnapshot(ReviewSnapshotDraft {
                window_start_ms: T0 - 7 * 86_400_000,
                window_end_ms: T0,
                totals: ReviewTotals::default(),
                streams: Vec::new(),
                streaks: Vec::new(),
                note: None,
            })
        }),
    ];

    // Failures are collected rather than asserted in place. A table that
    // panics on its first bad row says nothing about the other ten, and the
    // whole point of the table is that this is a *class*: reverting two of the
    // fixes has to name two cases, not one.
    let mut failures: Vec<String> = Vec::new();
    for (name, build) in cases {
        let e = engine_random_keys(ROOT, [7u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
        let mut db = db();
        // Deliberately no `ensure_base_epochs`. `Core::open` runs it, which is
        // why none of this is reachable in production; an engine built
        // directly is not so lucky, and that is every test that skips `Core`.
        let cmd = build(&mut db);
        let res = match e.apply(&mut db, cmd) {
            Ok(res) => res,
            Err(err) => {
                failures.push(format!("{name}: collided with the key it mints: {err:?}"));
                continue;
            }
        };

        let seqs = meta_seqs_of(&db, &e.keychain.device_id());
        // Without this the density check below could hold vacuously on a
        // transaction that never minted and so never raced anything.
        if seqs.len() <= 1 {
            failures.push(format!(
                "{name}: emitted only its own op, so nothing was raced: {seqs:?}"
            ));
            continue;
        }
        let dense: Vec<u64> = (1..=seqs.len() as u64).collect();
        if seqs != dense {
            failures.push(format!(
                "{name}: vault-meta seqs must be dense and unique, got {seqs:?}"
            ));
        }
        // Sequence 1 belongs to the first `key_envelope` the mint emitted. A
        // command that took it is a command that read its number before the
        // mint spent it — the bug in the form it would take if the UNIQUE
        // constraint ever stopped catching it.
        if res.seq <= 1 {
            failures.push(format!(
                "{name}: took seq {}, so it sequenced before the envelopes it caused",
                res.seq
            ));
        }
        // And the op is genuinely in the log rather than `OR IGNORE`d away.
        let logged: u32 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM ops WHERE op_id = ?",
                params![&res.op_id[..]],
                |r| r.get(0),
            )
            .unwrap();
        if logged != 1 {
            failures.push(format!("{name}: {logged} rows in the log for its op id"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} vault-meta commands race the key they mint:\n  {}",
        failures.len(),
        cases.len(),
        failures.join("\n  ")
    );
}

/// No command lets a caller name the vault-meta stream as an ordinary Stream.
///
/// `EntityRef::new` does not police the bytes, and every draft and patch below
/// crosses the UniFFI seam — so "you would have to construct it by hand" means
/// "any app, any CLI path, any future binding". `require_kind` checked the
/// *kind* and not the id, which was the whole of the gap: a `Stream` ref of
/// sixteen zero bytes is a well-formed Stream reference to the vault-meta log.
///
/// What it would have bought a caller: a Task routed into vault-meta reads the
/// sequence number the control ops are themselves counting, which is `#214`
/// from the other end — the half `Engine::log_slot` cannot close, because the
/// op is not a vault-meta *command* and has no business being there at all.
///
/// One command is deliberately **absent** from this table and must stay
/// absent: `Command::RotateStreamKey`. Rotating the vault-meta key is a real
/// operation — a device revocation does exactly that — and
/// `reserved_stream_is_still_rotatable` below pins it.
#[test]
fn no_command_accepts_the_vault_meta_stream_as_an_ordinary_target() {
    /// A well-formed `Stream` reference to the vault-meta log. Exactly what a
    /// binding could build, and what nothing rejected before.
    fn meta_ref() -> EntityRef {
        EntityRef::new(EntityKind::Stream, META_STREAM)
    }

    type Build = fn(&Engine, &mut Db) -> Command;
    let cases: &[(&str, Build)] = &[
        ("CreateTask.stream_id", |_e, _db| {
            Command::CreateTask(TaskDraft {
                title: "t".into(),
                stream_id: Some(meta_ref()),
                ..Default::default()
            })
        }),
        ("UpdateTask.patch.stream_id", |e, db| Command::UpdateTask {
            id: new_task(e, db, "t"),
            patch: TaskPatch {
                stream_id: Some(Some(meta_ref())),
                ..Default::default()
            },
        }),
        ("PromoteToStream.stream", |e, db| Command::PromoteToStream {
            id: new_task(e, db, "t"),
            stream: meta_ref(),
        }),
        ("CreateStream.parent_id", |_e, _db| {
            Command::CreateStream(StreamDraft {
                name: "child".into(),
                parent_id: Some(meta_ref()),
                ..Default::default()
            })
        }),
        ("UpdateStream.id", |_e, _db| Command::UpdateStream {
            id: meta_ref(),
            patch: StreamPatch {
                name: Some("renamed".into()),
                ..Default::default()
            },
        }),
        ("UpdateStream.patch.parent_id", |e, db| {
            let id = e
                .apply(
                    db,
                    Command::CreateStream(StreamDraft {
                        name: "child".into(),
                        ..Default::default()
                    }),
                )
                .unwrap()
                .entity;
            Command::UpdateStream {
                id,
                patch: StreamPatch {
                    parent_id: Some(Some(meta_ref())),
                    ..Default::default()
                },
            }
        }),
        ("DeleteStream.id", |_e, _db| Command::DeleteStream {
            id: meta_ref(),
            disposition: sunrise_domain::Disposition::Detach,
        }),
        ("CreateRoutine.template.stream_id", |_e, _db| {
            Command::CreateRoutine(routine_draft(
                meta_ref(),
                "FREQ=DAILY",
                T0 as i64,
                RoutineCatchupPolicy::Skip,
                Vec::new(),
            ))
        }),
        ("UpdateRoutine.patch.template", |e, db| {
            let d = routine_draft(
                inbox_stream_ref(),
                "FREQ=DAILY",
                T0 as i64,
                RoutineCatchupPolicy::Skip,
                Vec::new(),
            );
            let mut template = d.template.clone();
            let id = e.apply(db, Command::CreateRoutine(d)).unwrap().entity;
            template.stream_id = Some(meta_ref());
            Command::UpdateRoutine {
                scope: sunrise_domain::EditScope::All,
                id,
                patch: RoutinePatch {
                    template: Some(template),
                    ..Default::default()
                },
            }
        }),
        ("CreateBlock.stream_id", |_e, _db| {
            let mut d = block_draft(9, 1, Some("b"));
            d.stream_id = Some(meta_ref());
            Command::CreateBlock(d)
        }),
        ("ImportBlock.draft.stream_id", |_e, _db| {
            let mut draft = block_draft(9, 1, Some("b"));
            draft.stream_id = Some(meta_ref());
            Command::ImportBlock {
                source: "ics".into(),
                uid: "uid-1".into(),
                draft,
            }
        }),
        ("UpdateBlock.patch.stream_id", |e, db| {
            let id = e
                .apply(db, Command::CreateBlock(block_draft(9, 1, Some("b"))))
                .unwrap()
                .entity;
            Command::UpdateBlock {
                id,
                patch: BlockPatch {
                    stream_id: Some(Some(meta_ref())),
                    ..Default::default()
                },
            }
        }),
    ];

    let mut failures: Vec<String> = Vec::new();
    for (name, build) in cases {
        let mut db = db();
        let e = engine();
        e.ensure_base_epochs(&mut db).unwrap();
        let cmd = build(&e, &mut db);
        match e.apply(&mut db, cmd) {
            Err(EngineError::ReservedStream) => {}
            Err(other) => failures.push(format!("{name}: refused, but not by name: {other:?}")),
            Ok(res) => failures.push(format!(
                "{name}: accepted, and wrote {:?} at seq {}",
                res.entity, res.seq
            )),
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} caller-supplied stream ids reach the vault-meta log:\n  {}",
        failures.len(),
        cases.len(),
        failures.join("\n  ")
    );
}

/// The one command that may name the vault-meta stream, and the reason the
/// guard is a per-command decision rather than a blanket ban.
///
/// `RotateStreamKey` mints a new epoch and seals it to every current device.
/// On the vault-meta stream that is half of what a device revocation does, and
/// a user who believes the control log's key is exposed has to be able to ask
/// for it by name. It routes no entity op and reads its own sequence number
/// inside the transaction, so it competes with nothing.
#[test]
fn the_vault_meta_stream_is_still_rotatable_by_name() {
    let mut db = db();
    // A random-key engine, not the derived-key one: the derived-key test
    // keychain reports every key as already held and so writes no
    // `stream_epochs` row, which is the thing this test reads.
    let e = engine_random_keys(ROOT, [7u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    e.ensure_base_epochs(&mut db).unwrap();
    let before = db
        .with_tx(|tx| e.keychain.current_epoch_tx(tx, &META_STREAM))
        .unwrap()
        .expect("base epochs exist");
    e.apply(
        &mut db,
        Command::RotateStreamKey {
            stream: EntityRef::new(EntityKind::Stream, META_STREAM),
        },
    )
    .expect("rotating the control log's own key is a supported operation");
    let after = db
        .with_tx(|tx| e.keychain.current_epoch_tx(tx, &META_STREAM))
        .unwrap()
        .expect("still there");
    assert!(after > before, "the rotation minted a new epoch");
}

/// The `key_envelope` ops a mint emits now stamp **before** the op they
/// enable, and that reversal is the one observable consequence of taking the
/// LWW stamp inside the transaction rather than before it.
///
/// It is the causally correct order — the envelope carries the key the op is
/// sealed under — and it changes no merge outcome, because a control op
/// materializes no entity row and so is never the other side of an LWW
/// comparison. What it does mean is that HLC order and `seq` order now agree
/// within one transaction, where before they disagreed.
///
/// In production this never differs: `Core::open` runs `ensure_base_epochs`,
/// so the mint is a no-op and the transaction makes exactly one `hlc.send`
/// either way.
#[test]
fn a_mints_envelopes_stamp_before_the_op_that_caused_them() {
    let mut db = db();
    let e = engine_random_keys(ROOT, [7u8; 32], Arc::new(FakeClock(PLMutex::new(T0))));
    let res = e
        .apply(
            &mut db,
            Command::CreateStream(StreamDraft {
                name: "Work".into(),
                ..Default::default()
            }),
        )
        .unwrap();

    let mut stmt = db
        .conn()
        .prepare(
            "SELECT seq, envelope FROM ops
                 WHERE stream_id = ? AND device_id = ?
                 ORDER BY seq ASC",
        )
        .unwrap();
    let rows: Vec<(u64, Vec<u8>)> = stmt
        .query_map(
            params![&META_STREAM[..], &e.keychain.device_id()[..]],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(rows.len() > 1, "the mint emitted envelopes");

    let hlcs: Vec<Hlc> = rows
        .iter()
        .map(|(_, env)| sunrise_crypto::decode_envelope(env).unwrap().hlc)
        .collect();
    for w in hlcs.windows(2) {
        assert!(
            w[0] < w[1],
            "HLC order must follow seq order within one transaction: {:?} then {:?}",
            w[0],
            w[1]
        );
    }
    assert_eq!(
        rows.last().unwrap().0,
        res.seq,
        "the create holds the last seq, so it also holds the last HLC"
    );
}

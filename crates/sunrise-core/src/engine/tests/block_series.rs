//! A Block's kind, series and import fields on the wire and in storage
//! (#1147, `docs/02-domain/time-blocks.md` §Fields).
//!
//! No command writes a series yet (#1148), so these tests write the block op
//! as another build would, through [`log_and_fold`], and read back what the
//! projection holds.

use super::*;
use crate::feature::BLOCK_RECURRENCE_FEATURE;
use std::collections::BTreeMap;
use sunrise_domain::{Block, BlockException, BlockKind, BlockSource, NoteBody, RRule};

/// A block created by the command, in a legacy vault (full-state ops).
fn created(e: &Engine, db: &mut Db) -> Block {
    legacy_peer(db);
    let id = e
        .apply(db, Command::CreateBlock(block_draft(9, 1, Some("Standup"))))
        .unwrap()
        .entity;
    read_block(db.conn(), id.bytes()).unwrap().unwrap()
}

/// `b` with every #1147 field set, a cancelled and a moved occurrence among
/// its exceptions.
fn with_series(b: Block) -> Block {
    let (moved_start, moved_end) = block_at(11, 1);
    Block {
        kind: Some(BlockKind::Fixed),
        rrule: Some(RRule::parse("FREQ=DAILY;COUNT=5").unwrap()),
        exceptions: BTreeMap::from([
            ("2026-03-05T09:00".to_owned(), BlockException::cancelled()),
            (
                "2026-03-06T09:00".to_owned(),
                BlockException {
                    starts_at: Some(moved_start),
                    ends_at: Some(moved_end),
                    location: Some("Room 5".into()),
                    ..BlockException::default()
                },
            ),
        ]),
        split_from: Some(EntityRef::new(EntityKind::Block, [0x5a; 16])),
        location: Some("Room 4".into()),
        notes: Some(NoteBody(b"agenda".to_vec())),
        source: Some(BlockSource::from_raw("import:caldav")),
        external_id: Some("uid@example.com".into()),
        ..b
    }
}

fn write(e: &Engine, db: &mut Db, b: &Block) -> Result<(), EngineError> {
    let stream = crate::engine::ids::domain_bytes(b.stream_id);
    let now_ms = e.clock.now_ms();
    let op = InnerOp::BlockUpdate(Box::new(b.clone()));
    db.with_tx(|tx| e.append_and_fold(tx, &op, &stream, now_ms).map(|_| ()))
        .map_err(EngineError::from)
        .map_err(EngineError::lift_seal_refusal)
}

/// An op carrying a series is refused at the seal until the vault requires
/// `block.recurrence`, and writes nothing; the additive fields need no
/// feature. Once required, every field lands in its column and reads back as
/// written, and a rebuild from the log reproduces it.
#[test]
fn a_series_is_sealed_only_once_the_vault_requires_block_recurrence() {
    let e = engine();
    let mut db = db();
    let b = created(&e, &mut db);
    let ops = op_count(&db);

    let series = with_series(b.clone());
    let err = write(&e, &mut db, &series).unwrap_err();
    assert!(
        matches!(&err, EngineError::Invalid(m) if m.contains(BLOCK_RECURRENCE_FEATURE)),
        "{err:?}"
    );
    assert_eq!(op_count(&db), ops, "the refused op wrote nothing");
    assert_eq!(read_block(db.conn(), b.id.bytes()).unwrap().unwrap(), b);

    let additive = Block {
        rrule: None,
        exceptions: BTreeMap::new(),
        split_from: None,
        ..series.clone()
    };
    write(&e, &mut db, &additive).unwrap();
    assert_eq!(
        read_block(db.conn(), b.id.bytes()).unwrap().unwrap(),
        additive
    );

    e.require_features(&mut db, &[BLOCK_RECURRENCE_FEATURE], true)
        .unwrap();
    write(&e, &mut db, &series).unwrap();
    let stored = read_block(db.conn(), b.id.bytes()).unwrap().unwrap();
    assert_eq!(stored, series);
    assert_eq!(stored.effective_kind(), BlockKind::Fixed);
    assert!(stored.series().is_some());
    e.assert_rebuild_identity(&db);
}

/// An absent kind is stored as NULL, not as the kind it reads as, and an
/// unknown one is stored by its spelling.
#[test]
fn the_kind_column_holds_what_the_op_carried() {
    let e = engine();
    let mut db = db();
    let b = created(&e, &mut db);
    let column = |db: &Db| -> Option<String> {
        db.conn()
            .query_row(
                "SELECT kind FROM blocks WHERE id = ?",
                [&b.id.bytes()[..]],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(column(&db), None);
    assert_eq!(b.effective_kind(), BlockKind::Flexible);

    let tentative = Block {
        kind: Some(BlockKind::from_raw("tentative")),
        ..b
    };
    write(&e, &mut db, &tentative).unwrap();
    assert_eq!(column(&db).as_deref(), Some("tentative"));
    let stored = read_block(db.conn(), tentative.id.bytes())
        .unwrap()
        .unwrap();
    assert_eq!(stored.kind, tentative.kind);
    assert_eq!(stored.effective_kind(), BlockKind::Fixed);
}

/// In a `core.field_ops` vault `exceptions` is a map of registers: two
/// devices that each change a different occurrence both keep their change,
/// and a write that drops one key tombstones only that key.
#[test]
fn two_devices_editing_two_occurrences_both_keep_their_edit() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);
    ea.advertise_features(&mut dba).unwrap();
    eb.advertise_features(&mut dbb).unwrap();
    let sync = |dba: &mut Db, dbb: &mut Db| {
        for env in all_envelopes(dba) {
            let _ = eb.apply_remote(dbb, &env);
        }
        for env in all_envelopes(dbb) {
            let _ = ea.apply_remote(dba, &env);
        }
    };
    sync(&mut dba, &mut dbb);

    let id = ea
        .apply(
            &mut dba,
            Command::CreateBlock(block_draft(9, 1, Some("Standup"))),
        )
        .unwrap()
        .entity;
    ea.require_features(&mut dba, &[BLOCK_RECURRENCE_FEATURE], false)
        .unwrap();
    let base = Block {
        rrule: Some(RRule::parse("FREQ=DAILY;COUNT=5").unwrap()),
        ..read_block(dba.conn(), id.bytes()).unwrap().unwrap()
    };
    set_clock(&ca, T0 + 1_000);
    write(&ea, &mut dba, &base).unwrap();
    sync(&mut dba, &mut dbb);

    let k1 = "2026-03-05T09:00".to_owned();
    let k2 = "2026-03-06T09:00".to_owned();
    set_clock(&ca, T0 + 2_000);
    let on_a = Block {
        exceptions: BTreeMap::from([(k1.clone(), BlockException::cancelled())]),
        ..base.clone()
    };
    write(&ea, &mut dba, &on_a).unwrap();
    set_clock(&cb, T0 + 3_000);
    let moved = BlockException {
        title: Some("Moved".into()),
        ..BlockException::default()
    };
    let on_b = Block {
        exceptions: BTreeMap::from([(k2.clone(), moved.clone())]),
        ..base.clone()
    };
    write(&eb, &mut dbb, &on_b).unwrap();
    // Each edit is a `Patch` of the one key it changed, not a full state.
    let last_kind: String = dbb
        .conn()
        .query_row(
            "SELECT inner_kind FROM ops ORDER BY rowid DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(last_kind, "block.patch");
    sync(&mut dba, &mut dbb);

    let both = BTreeMap::from([
        (k1.clone(), BlockException::cancelled()),
        (k2.clone(), moved.clone()),
    ]);
    let a = read_block(dba.conn(), id.bytes()).unwrap().unwrap();
    let b = read_block(dbb.conn(), id.bytes()).unwrap().unwrap();
    assert_eq!(a.exceptions, both);
    assert_eq!(b.exceptions, both);

    // Dropping one occurrence's exception leaves the other's.
    set_clock(&ca, T0 + 4_000);
    write(
        &ea,
        &mut dba,
        &Block {
            exceptions: BTreeMap::from([(k2.clone(), moved.clone())]),
            ..a
        },
    )
    .unwrap();
    sync(&mut dba, &mut dbb);
    let only = BTreeMap::from([(k2, moved)]);
    assert_eq!(
        read_block(dbb.conn(), id.bytes())
            .unwrap()
            .unwrap()
            .exceptions,
        only
    );
    assert_eq!(
        read_block(dba.conn(), id.bytes())
            .unwrap()
            .unwrap()
            .exceptions,
        only
    );
    ea.assert_rebuild_identity(&dba);
    eb.assert_rebuild_identity(&dbb);
}

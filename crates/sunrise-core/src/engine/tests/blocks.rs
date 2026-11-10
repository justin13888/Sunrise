//! Time blocks and the blocks an import writes.

use super::*;

// ---- time blocks (docs/02-domain/time-blocks.md) ----

#[test]
fn create_block_round_trips_and_lands_on_the_day_grid() {
    let mut db = db();
    legacy_peer(&db);
    let e = engine();
    let res = e
        .apply(
            &mut db,
            Command::CreateBlock(block_draft(9, 1, Some("Gym"))),
        )
        .unwrap();
    assert_eq!(res.entity.kind(), EntityKind::Block);

    let rows = day_rows(&e, &db, day_ms());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].block.id, res.entity);
    assert_eq!(rows[0].title.as_deref(), Some("Gym"));
    assert!(rows[0].block.tasks.is_empty());

    // The op is a real sealed `BlockCreate`.
    assert_eq!(inner_op_variant(&e, &db, &res.op_id), "BlockCreate");
}

#[test]
fn an_inverted_block_is_rejected() {
    let mut db = db();
    let e = engine();
    let (starts_at, ends_at) = block_at(10, 1);
    let bad = BlockDraft {
        starts_at: ends_at,
        ends_at: starts_at,
        ..block_draft(9, 1, Some("backwards"))
    };
    assert!(matches!(
        e.apply(&mut db, Command::CreateBlock(bad)),
        Err(EngineError::Validation(ValidationError::Field {
            field: "block.ends_at",
            ..
        }))
    ));
}

/// The grid asks "what overlaps this day", so a block that started
/// yesterday and runs into today is on today's grid — and one that ends
/// exactly at midnight is not.
#[test]
fn the_day_grid_returns_overlaps_not_containments() {
    let mut db = db();
    let e = engine();
    let midnight = jiff::civil::date(2026, 3, 4).at(0, 0, 0, 0);
    let spans_into_today = BlockDraft {
        starts_at: SunriseTime::floating(jiff::civil::date(2026, 3, 3).at(23, 0, 0, 0)),
        ends_at: SunriseTime::floating(jiff::civil::date(2026, 3, 4).at(1, 0, 0, 0)),
        ..block_draft(9, 1, Some("overnight"))
    };
    let ends_at_midnight = BlockDraft {
        starts_at: SunriseTime::floating(jiff::civil::date(2026, 3, 3).at(22, 0, 0, 0)),
        ends_at: SunriseTime::floating(midnight),
        ..block_draft(9, 1, Some("yesterday only"))
    };
    e.apply(&mut db, Command::CreateBlock(spans_into_today))
        .unwrap();
    e.apply(&mut db, Command::CreateBlock(ends_at_midnight))
        .unwrap();

    let titles: Vec<_> = day_rows(&e, &db, day_ms())
        .into_iter()
        .filter_map(|r| r.title)
        .collect();
    assert_eq!(titles, vec!["overnight".to_string()]);
}

/// The titles `Query::WeekBlocks` returns for the week of `day_ms()`.
fn week_titles(e: &Engine, db: &Db) -> Vec<String> {
    match e
        .query(
            db,
            Query::WeekBlocks {
                week_ms: day_ms(), // Wednesday 2026-03-04
            },
        )
        .unwrap()
    {
        QueryResult::Blocks(rows) => rows.into_iter().filter_map(|r| r.title).collect(),
        other => panic!("expected blocks, got {other:?}"),
    }
}

/// A week starts on the `week_start` preference, Sunday by default
/// (`docs/10-cross-cutting/time.md` §4), and a device that says Monday gets
/// Monday. Nothing hard-codes a first weekday. 2026-03-04 is a Wednesday.
#[test]
fn the_week_grid_starts_on_the_week_start_preference() {
    let mut db = db();
    let e = engine();
    for (date, title) in [
        ((2026, 3, 1), "sunday"),           // in a Sunday week
        ((2026, 3, 2), "monday"),           // in both
        ((2026, 3, 7), "saturday"),         // in both
        ((2026, 3, 8), "next_sunday"),      // in a Monday week
        ((2026, 3, 9), "next_monday"),      // in neither
        ((2026, 2, 28), "saturday_before"), // in neither
    ] {
        let d = jiff::civil::date(date.0, date.1, date.2);
        e.apply(
            &mut db,
            Command::CreateBlock(BlockDraft {
                starts_at: SunriseTime::floating(d.at(9, 0, 0, 0)),
                ends_at: SunriseTime::floating(d.at(10, 0, 0, 0)),
                ..block_draft(9, 1, Some(title))
            }),
        )
        .unwrap();
    }
    assert_eq!(week_titles(&e, &db), ["sunday", "monday", "saturday"]);
    e.apply(
        &mut db,
        Command::SetPreference {
            key: "week_start".into(),
            value: sunrise_domain::PrefValue::Weekday(sunrise_domain::Weekday::Mo),
            target: sunrise_domain::PrefTarget::Vault,
        },
    )
    .unwrap();
    assert_eq!(week_titles(&e, &db), ["monday", "saturday", "next_sunday"]);
}

/// A week window is built from civil dates, so it is exactly seven days
/// whatever the transitions inside it. 2026-03-04 is a Wednesday.
#[test]
fn the_week_grid_runs_monday_to_sunday() {
    let mut db = db();
    let e = engine();
    e.apply(
        &mut db,
        Command::SetPreference {
            key: "week_start".into(),
            value: sunrise_domain::PrefValue::Weekday(sunrise_domain::Weekday::Mo),
            target: sunrise_domain::PrefTarget::Vault,
        },
    )
    .unwrap();
    for (date, title) in [
        ((2026, 3, 2), "monday"),        // in
        ((2026, 3, 8), "sunday"),        // in
        ((2026, 3, 1), "sunday_before"), // out
        ((2026, 3, 9), "next_monday"),   // out
    ] {
        let d = jiff::civil::date(date.0, date.1, date.2);
        e.apply(
            &mut db,
            Command::CreateBlock(BlockDraft {
                starts_at: SunriseTime::floating(d.at(9, 0, 0, 0)),
                ends_at: SunriseTime::floating(d.at(10, 0, 0, 0)),
                ..block_draft(9, 1, Some(title))
            }),
        )
        .unwrap();
    }
    let rows = match e
        .query(
            &db,
            Query::WeekBlocks {
                week_ms: day_ms(), // Wednesday of that week
            },
        )
        .unwrap()
    {
        QueryResult::Blocks(rows) => rows,
        other => panic!("expected blocks, got {other:?}"),
    };
    let titles: Vec<_> = rows.into_iter().filter_map(|r| r.title).collect();
    assert_eq!(titles, vec!["monday".to_string(), "sunday".to_string()]);
}

#[test]
fn binding_a_task_updates_task_blocks_symmetrically() {
    let mut db = db();
    let e = engine();
    let task = new_task(&e, &mut db, "Write the report");
    let block = e
        .apply(
            &mut db,
            Command::CreateBlock(block_draft(9, 2, Some("Deep work"))),
        )
        .unwrap()
        .entity;

    e.apply(&mut db, Command::BindTask { block, task }).unwrap();

    // Forward: the block carries the task.
    let rows = day_rows(&e, &db, day_ms());
    assert_eq!(
        rows[0].block.tasks.iter().copied().collect::<Vec<_>>(),
        vec![task]
    );
    assert_eq!(rows[0].task_titles, vec!["Write the report".to_string()]);
    // Reverse: the task carries the block, with no second op.
    let t = read_task(db.conn(), task.bytes()).unwrap().unwrap();
    assert_eq!(t.blocks.iter().copied().collect::<Vec<_>>(), vec![block]);

    // Unbinding takes both halves back down.
    e.apply(&mut db, Command::UnbindTask { block, task })
        .unwrap();
    let t = read_task(db.conn(), task.bytes()).unwrap().unwrap();
    assert!(t.blocks.is_empty());
    assert!(day_rows(&e, &db, day_ms())[0].block.tasks.is_empty());
}

/// The spec's shadow copy: the title is taken at bind time and does NOT
/// follow a later rename of the task.
#[test]
fn an_untitled_block_shadow_copies_the_bound_task_title() {
    let mut db = db();
    let e = engine();
    let task = new_task(&e, &mut db, "Original title");
    let block = e
        .apply(&mut db, Command::CreateBlock(block_draft(9, 1, None)))
        .unwrap()
        .entity;
    e.apply(&mut db, Command::BindTask { block, task }).unwrap();
    assert_eq!(
        day_rows(&e, &db, day_ms())[0].title.as_deref(),
        Some("Original title")
    );

    e.apply(
        &mut db,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                title: Some("Renamed later".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    assert_eq!(
        day_rows(&e, &db, day_ms())[0].title.as_deref(),
        Some("Original title"),
        "a shadow copy does not follow the task"
    );
}

#[test]
fn title_track_task_follows_the_rename() {
    let mut db = db();
    let e = engine();
    let task = new_task(&e, &mut db, "Original title");
    let block = e
        .apply(
            &mut db,
            Command::CreateBlock(BlockDraft {
                title_track_task: true,
                tasks: vec![task],
                ..block_draft(9, 1, None)
            }),
        )
        .unwrap()
        .entity;
    assert_eq!(
        day_rows(&e, &db, day_ms())[0].title.as_deref(),
        Some("Original title")
    );

    e.apply(
        &mut db,
        Command::UpdateTask {
            id: task,
            patch: TaskPatch {
                title: Some("Renamed later".into()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    assert_eq!(
        day_rows(&e, &db, day_ms())[0].title.as_deref(),
        Some("Renamed later")
    );

    // A second bound task takes the flag out of play: there is no single
    // task title left to shadow, so the STORED title stands — which is the
    // shadow copy taken when the block was created, not the rename.
    let other = new_task(&e, &mut db, "Something else");
    e.apply(&mut db, Command::BindTask { block, task: other })
        .unwrap();
    assert_eq!(
        day_rows(&e, &db, day_ms())[0].title.as_deref(),
        Some("Original title"),
        "the stored title stands once two tasks are bound"
    );
}

#[test]
fn updating_a_block_moves_it_on_the_grid() {
    let mut db = db();
    let e = engine();
    let block = e
        .apply(
            &mut db,
            Command::CreateBlock(block_draft(9, 1, Some("Gym"))),
        )
        .unwrap()
        .entity;
    let (starts_at, ends_at) = block_at(18, 1);
    e.apply(
        &mut db,
        Command::UpdateBlock {
            id: block,
            scope: sunrise_domain::EditScope::All,
            patch: BlockPatch {
                starts_at: Some(starts_at.clone()),
                ends_at: Some(ends_at),
                title: Some(Some("Evening gym".into())),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let rows = day_rows(&e, &db, day_ms());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title.as_deref(), Some("Evening gym"));
    assert_eq!(rows[0].block.starts_at, starts_at);
}

#[test]
fn a_patch_that_inverts_the_range_is_rejected() {
    let mut db = db();
    let e = engine();
    let block = e
        .apply(
            &mut db,
            Command::CreateBlock(block_draft(9, 1, Some("Gym"))),
        )
        .unwrap()
        .entity;
    let (before_start, _) = block_at(8, 1);
    assert!(matches!(
        e.apply(
            &mut db,
            Command::UpdateBlock {
                id: block,
                scope: sunrise_domain::EditScope::All,
                patch: BlockPatch {
                    ends_at: Some(before_start),
                    ..Default::default()
                },
            },
        ),
        Err(EngineError::Validation(ValidationError::Field {
            field: "block.ends_at",
            constraint: "after_starts_at",
        }))
    ));
}

#[test]
fn deleting_a_block_clears_it_from_the_grid_and_from_its_tasks() {
    let mut db = db();
    legacy_peer(&db);
    let e = engine();
    let task = new_task(&e, &mut db, "Write the report");
    let block = e
        .apply(
            &mut db,
            Command::CreateBlock(BlockDraft {
                tasks: vec![task],
                ..block_draft(9, 1, Some("Deep work"))
            }),
        )
        .unwrap()
        .entity;
    assert_eq!(day_rows(&e, &db, day_ms()).len(), 1);

    let res = e
        .apply(
            &mut db,
            Command::DeleteBlock {
                id: block,
                scope: sunrise_domain::EditScope::All,
            },
        )
        .unwrap();
    assert_eq!(inner_op_variant(&e, &db, &res.op_id), "BlockDelete");
    assert!(day_rows(&e, &db, day_ms()).is_empty());
    // The Task survives; only the binding stops being visible.
    let t = read_task(db.conn(), task.bytes()).unwrap().unwrap();
    assert!(!t.deleted);
    assert!(t.blocks.is_empty());
}

/// The delete-wins case, forced rather than raced.
///
/// A `BlockDelete` that beats a concurrent `BlockUpdate` has to replace the
/// **whole** row on the receiving replica, not just flip its tombstone. The
/// old id-only shape left B holding its own title under A's winning stamp,
/// and because both sides then carried that same stamp neither would ever
/// accept a correction (ADR-0014).
///
/// The e2e probes reproduce this only when the delete happens to land last.
/// Here the two stamps are chosen so it always does, which is what makes a
/// single run conclusive — and makes reverting the fix fail every time
/// rather than one run in four.
#[test]
fn a_block_delete_that_wins_lww_replaces_the_whole_row() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb.clone());
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);
    trust(&ea, &mut dba, &eb);

    let block = ea
        .apply(
            &mut dba,
            Command::CreateBlock(block_draft(9, 1, Some("contested"))),
        )
        .unwrap()
        .entity;
    eb.apply_remote(&mut dbb, &env_for_kind(&dba, block.bytes(), "block.create"))
        .unwrap();

    // B renames it...
    set_clock(&cb, T0 + 1_000);
    eb.apply(
        &mut dbb,
        Command::UpdateBlock {
            id: block,
            scope: sunrise_domain::EditScope::All,
            patch: BlockPatch {
                title: Some(Some("renamed".into())),
                ..Default::default()
            },
        },
    )
    .unwrap();

    // ...and A deletes it strictly later, so the delete wins on both sides.
    set_clock(&ca, T0 + 2_000);
    ea.apply(
        &mut dba,
        Command::DeleteBlock {
            id: block,
            scope: sunrise_domain::EditScope::All,
        },
    )
    .unwrap();

    eb.apply_remote(&mut dbb, &env_for_kind(&dba, block.bytes(), "block.delete"))
        .unwrap();
    ea.apply_remote(&mut dba, &env_for_kind(&dbb, block.bytes(), "block.update"))
        .unwrap();

    let ra = read_block(dba.conn(), block.bytes()).unwrap().unwrap();
    let rb = read_block(dbb.conn(), block.bytes()).unwrap().unwrap();
    assert!(ra.deleted && rb.deleted, "both replicas tombstoned it");
    assert_eq!(
        ra.title, rb.title,
        "and agree on the rest of the row, not just the tombstone"
    );
    assert_eq!(
        ra.title.as_deref(),
        Some("contested"),
        "the deleting replica's state is what replaced it"
    );
    assert_eq!(
        row_stamp(&dba, "blocks", "id", &block),
        row_stamp(&dbb, "blocks", "id", &block),
    );
}

/// A `BlockDelete` that overtakes its own `BlockCreate` materializes the
/// tombstone anyway. Under the old shape there was no row to tombstone, so
/// the delete was dropped, the create then landed live, and the replica sat
/// permanently out of step with the one that had deleted.
#[test]
fn a_block_delete_that_overtakes_its_create_still_lands_as_a_tombstone() {
    let ca = Arc::new(FakeClock(PLMutex::new(T0)));
    let cb = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], ca.clone());
    let eb = engine_seeded(ROOT, [2u8; 32], cb);
    let mut dba = db_root(ROOT);
    legacy_peer(&dba);
    let mut dbb = db_root(ROOT);
    legacy_peer(&dbb);
    trust(&eb, &mut dbb, &ea);

    let block = ea
        .apply(
            &mut dba,
            Command::CreateBlock(block_draft(9, 1, Some("Deep work"))),
        )
        .unwrap()
        .entity;
    set_clock(&ca, T0 + 1_000);
    ea.apply(
        &mut dba,
        Command::DeleteBlock {
            id: block,
            scope: sunrise_domain::EditScope::All,
        },
    )
    .unwrap();

    // Delete first, create second.
    eb.apply_remote(&mut dbb, &env_for_kind(&dba, block.bytes(), "block.delete"))
        .unwrap();
    let rb = read_block(dbb.conn(), block.bytes())
        .unwrap()
        .expect("the delete materialized a row of its own");
    assert!(rb.deleted);
    assert_eq!(rb.title.as_deref(), Some("Deep work"));

    eb.apply_remote(&mut dbb, &env_for_kind(&dba, block.bytes(), "block.create"))
        .unwrap();
    assert!(
        read_block(dbb.conn(), block.bytes())
            .unwrap()
            .unwrap()
            .deleted,
        "the older create loses LWW and does not resurrect it"
    );
    assert_eq!(
        row_stamp(&dba, "blocks", "id", &block),
        row_stamp(&dbb, "blocks", "id", &block),
    );
}

#[test]
fn block_commands_reject_a_non_block_id() {
    let mut db = db();
    let e = engine();
    let task = new_task(&e, &mut db, "not a block");
    invalid(
        e.apply(
            &mut db,
            Command::DeleteBlock {
                id: task,
                scope: sunrise_domain::EditScope::All,
            },
        ),
        "expected Block, got Task",
    );
    invalid(
        e.apply(
            &mut db,
            Command::UpdateBlock {
                id: task,
                scope: sunrise_domain::EditScope::All,
                patch: BlockPatch::default(),
            },
        ),
        "expected Block, got Task",
    );
    invalid(
        e.apply(&mut db, Command::BindTask { block: task, task }),
        "expected Block, got Task",
    );
}

#[test]
fn entity_by_id_reads_one_block_as_a_grid_row() {
    let mut db = db();
    let e = engine();
    let block = e
        .apply(
            &mut db,
            Command::CreateBlock(block_draft(9, 1, Some("Gym"))),
        )
        .unwrap()
        .entity;
    match e.query(&db, Query::EntityById(block)).unwrap() {
        QueryResult::Blocks(rows) => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].block.id, block);
        }
        other => panic!("expected blocks, got {other:?}"),
    }
}

/// A block created on one replica materializes identically on another from
/// the sealed op alone — bindings included.
#[test]
fn a_remote_block_op_materializes_with_its_bindings() {
    let clock_a = Arc::new(FakeClock(PLMutex::new(T0)));
    let clock_b = Arc::new(FakeClock(PLMutex::new(T0)));
    let ea = engine_seeded(ROOT, [1u8; 32], clock_a);
    let eb = engine_seeded(ROOT, [2u8; 32], clock_b);
    let mut dba = db_root(ROOT);
    let mut dbb = db_root(ROOT);
    trust(&eb, &mut dbb, &ea);

    let task = new_task(&ea, &mut dba, "Write the report");
    let created = ea
        .apply(
            &mut dba,
            Command::CreateBlock(BlockDraft {
                tasks: vec![task],
                ..block_draft(9, 1, Some("Deep work"))
            }),
        )
        .unwrap();

    let event = eb
        .apply_remote(&mut dbb, &env_bytes(&dba, &created.op_id))
        .unwrap();
    assert!(matches!(event, Some(DomainEvent::Created(id)) if id == created.entity));

    let rows = day_rows(&eb, &dbb, day_ms());
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].block.tasks.iter().copied().collect::<Vec<_>>(),
        vec![task],
        "the binding rides along with the full-state op"
    );
    // The bound task's own op has not arrived on this replica, so there is
    // no title to report for it — a valid state, not a repair case.
    assert!(rows[0].task_titles.is_empty());
    assert_eq!(rows[0].title.as_deref(), Some("Deep work"));
}

// ---- imported blocks (docs/09-integrations/icalendar.md §Import) ----

/// The rule the whole importer rests on: the same `(source, uid)` is the
/// same Block, not a second one.
#[test]
fn importing_the_same_uid_twice_writes_one_block() {
    let mut db = db();
    let e = engine();
    let first = e
        .apply(
            &mut db,
            import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
        )
        .unwrap();
    let second = e
        .apply(
            &mut db,
            import("ics", "ev1", block_draft(10, 1, Some("Gym, moved"))),
        )
        .unwrap();
    assert_eq!(first.entity, second.entity);
    assert_eq!(first.entity, imported_block_id("ics", "ev1"));

    let rows = day_rows(&e, &db, day_ms());
    assert_eq!(rows.len(), 1, "one block, not two: {rows:?}");
    assert_eq!(rows[0].title.as_deref(), Some("Gym, moved"));
    assert_eq!(rows[0].block.starts_at, block_at(10, 1).0);
}

#[test]
fn the_same_uid_from_two_sources_is_two_blocks() {
    let mut db = db();
    let e = engine();
    let a = e
        .apply(&mut db, import("ics", "ev1", block_draft(9, 1, Some("A"))))
        .unwrap();
    let b = e
        .apply(
            &mut db,
            import("other", "ev1", block_draft(11, 1, Some("B"))),
        )
        .unwrap();
    assert_ne!(a.entity, b.entity);
    assert_eq!(day_rows(&e, &db, day_ms()).len(), 2);
}

/// A re-import is not a second creation, so the Block keeps the moment it
/// first appeared.
#[test]
fn a_re_import_preserves_created_at_and_moves_updated_at() {
    let clock = Arc::new(FakeClock(PLMutex::new(T0)));
    let e = engine_seeded(ROOT, [1u8; 32], clock.clone());
    let mut db = db_root(ROOT);
    legacy_peer(&db);
    let id = e
        .apply(
            &mut db,
            import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
        )
        .unwrap()
        .entity;
    set_clock(&clock, T0 + 60_000);
    e.apply(
        &mut db,
        import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
    )
    .unwrap();

    let b = read_block(db.conn(), id.bytes()).unwrap().unwrap();
    assert_eq!(b.created_at.as_millisecond(), T0 as i64);
    assert_eq!(b.updated_at.as_millisecond(), (T0 + 60_000) as i64);
}

/// A task the user bound to an imported Block is theirs. A re-import
/// carries no bindings and must not take it away.
#[test]
fn a_re_import_keeps_a_task_the_user_bound() {
    let mut db = db();
    let e = engine();
    let block = e
        .apply(
            &mut db,
            import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
        )
        .unwrap()
        .entity;
    let task = e
        .apply(
            &mut db,
            Command::CreateTask(TaskDraft {
                title: "Pack kit".into(),
                ..Default::default()
            }),
        )
        .unwrap()
        .entity;
    e.apply(&mut db, Command::BindTask { block, task }).unwrap();

    e.apply(
        &mut db,
        import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
    )
    .unwrap();
    let rows = day_rows(&e, &db, day_ms());
    assert_eq!(
        rows[0].block.tasks.iter().copied().collect::<Vec<_>>(),
        vec![task],
        "the user's binding survives a re-import"
    );
}

/// The op kind is what the activity feed reads: a first import is a
/// creation and a re-import is not.
#[test]
fn the_first_import_emits_a_create_and_the_second_an_update() {
    let mut db = db();
    legacy_peer(&db);
    let e = engine();
    let first = e
        .apply(
            &mut db,
            import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
        )
        .unwrap();
    assert_eq!(inner_op_variant(&e, &db, &first.op_id), "BlockCreate");
    let second = e
        .apply(
            &mut db,
            import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
        )
        .unwrap();
    assert_eq!(inner_op_variant(&e, &db, &second.op_id), "BlockUpdate");
}

/// Re-importing an event the user deleted locally brings it back: the file
/// is the statement of what the external calendar holds, and honouring the
/// tombstone would make the import silently incomplete.
#[test]
fn a_re_import_resurrects_a_locally_deleted_block() {
    let mut db = db();
    let e = engine();
    let id = e
        .apply(
            &mut db,
            import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
        )
        .unwrap()
        .entity;
    e.apply(
        &mut db,
        Command::DeleteBlock {
            id,
            scope: sunrise_domain::EditScope::All,
        },
    )
    .unwrap();
    assert!(day_rows(&e, &db, day_ms()).is_empty());

    e.apply(
        &mut db,
        import("ics", "ev1", block_draft(9, 1, Some("Gym"))),
    )
    .unwrap();
    assert_eq!(day_rows(&e, &db, day_ms()).len(), 1);
}

#[test]
fn an_import_with_no_uid_is_refused() {
    let mut db = db();
    let e = engine();
    invalid(
        e.apply(&mut db, import("ics", "  ", block_draft(9, 1, Some("Gym")))),
        "an external item needs a UID",
    );
}

#[test]
fn an_import_validates_its_draft_like_any_other_write() {
    let mut db = db();
    let e = engine();
    let inverted = BlockDraft {
        starts_at: block_at(10, 1).0,
        ends_at: block_at(9, 1).0,
        ..block_draft(9, 1, Some("Gym"))
    };
    assert!(matches!(
        e.apply(&mut db, import("ics", "ev1", inverted)),
        Err(EngineError::Validation(ValidationError::Field {
            field: "block.ends_at",
            constraint: "after_starts_at",
        }))
    ));
}

// ---- recurring blocks (time-blocks.md §Recurring blocks, #1148) ----

use crate::feature::BLOCK_RECURRENCE_FEATURE;
use sunrise_domain::{split_block_id, Block, EditScope};

/// Give block `id` the rule `rule`, as a build that writes series would:
/// no command creates one yet.
fn recur(e: &Engine, db: &mut Db, id: EntityRef, rule: &str) {
    let b = Block {
        rrule: Some(RRule::parse(rule).unwrap()),
        ..read_block(db.conn(), id.bytes()).unwrap().unwrap()
    };
    let stream = crate::engine::ids::domain_bytes(b.stream_id);
    let now_ms = e.clock.now_ms();
    let op = InnerOp::BlockUpdate(Box::new(b));
    db.with_tx(|tx| e.append_and_fold(tx, &op, &stream, now_ms).map(|_| ()))
        .unwrap();
}

/// A "Standup" every Wednesday 09:00–10:00 (floating) from 2026-03-04, in
/// a vault that requires `block.recurrence`.
fn weekly(e: &Engine, db: &mut Db) -> EntityRef {
    legacy_peer(db);
    let id = e
        .apply(db, Command::CreateBlock(block_draft(9, 1, Some("Standup"))))
        .unwrap()
        .entity;
    e.require_features(db, &[BLOCK_RECURRENCE_FEATURE], true)
        .unwrap();
    recur(e, db, id, "FREQ=WEEKLY");
    id
}

/// Noon on 2026-03-`day`, for a day query.
fn march(day: i8) -> u64 {
    SunriseTime::floating(jiff::civil::date(2026, 3, day).at(12, 0, 0, 0))
        .to_instant(&jiff::tz::TimeZone::UTC)
        .unwrap()
        .as_millisecond() as u64
}

/// 2026-03-`day` at `hour`:00, floating.
fn at(day: i8, hour: i8) -> SunriseTime {
    SunriseTime::floating(jiff::civil::date(2026, 3, day).at(hour, 0, 0, 0))
}

/// What the day grid of 2026-03-`day` shows: each row's block, title and
/// occurrence key.
fn day(e: &Engine, db: &Db, day: i8) -> Vec<(EntityRef, Option<String>, Option<String>)> {
    day_rows(e, db, march(day))
        .into_iter()
        .map(|r| (r.block.id, r.title, r.occurrence_key))
        .collect()
}

fn key(day: i8) -> String {
    format!("2026-03-{day:02}T09:00")
}

fn scoped_update(id: EntityRef, scope: EditScope, patch: BlockPatch) -> Command {
    Command::UpdateBlock { id, scope, patch }
}

fn retitle(title: &str) -> BlockPatch {
    BlockPatch {
        title: Some(Some(title.into())),
        ..BlockPatch::default()
    }
}

/// A weekly block is one row per week, each at its own date with its key,
/// and nothing per occurrence is stored.
#[test]
fn a_weekly_block_is_one_row_per_week_in_the_window_with_its_key() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let rows = day_rows(&e, &db, march(11));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].block.id, id);
    assert_eq!(rows[0].occurrence_key, Some(key(11)));
    assert_eq!(rows[0].block.starts_at, at(11, 9));
    assert_eq!(rows[0].block.ends_at, at(11, 10));
    assert!(day(&e, &db, 12).is_empty(), "a Thursday has no standup");

    let from = march(1) as i64;
    let to = from + 28 * 86_400_000;
    let QueryResult::Blocks(rows) = e.query_blocks_between(&db, from, to).unwrap() else {
        panic!("expected blocks");
    };
    let keys: Vec<_> = rows.into_iter().filter_map(|r| r.occurrence_key).collect();
    assert_eq!(keys, vec![key(4), key(11), key(18), key(25)]);

    let stored: i64 = db
        .conn()
        .query_row("SELECT count(*) FROM blocks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, 1);
}

/// A one-off block keeps no key, and reading a recurring one by id returns
/// the series as stored.
#[test]
fn a_one_off_block_and_a_block_read_by_id_carry_no_key() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let once = e
        .apply(
            &mut db,
            Command::CreateBlock(block_draft(14, 1, Some("Gym"))),
        )
        .unwrap()
        .entity;
    let rows = day(&e, &db, 4);
    assert_eq!(
        rows,
        vec![
            (id, Some("Standup".into()), Some(key(4))),
            (once, Some("Gym".into()), None),
        ]
    );
}

/// *This* writes the occurrence's exception and nothing else: the other
/// weeks keep the series' title and time.
#[test]
fn an_edit_of_this_occurrence_changes_that_week_only() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let patch = BlockPatch {
        starts_at: Some(at(11, 10)),
        ends_at: Some(at(11, 11)),
        ..retitle("Moved standup")
    };
    let res = e
        .apply(
            &mut db,
            scoped_update(id, EditScope::This { key: key(11) }, patch),
        )
        .unwrap();
    assert_eq!(res.entity, id);

    let rows = day_rows(&e, &db, march(11));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title.as_deref(), Some("Moved standup"));
    assert_eq!(rows[0].block.starts_at, at(11, 10));
    assert_eq!(rows[0].occurrence_key, Some(key(11)));
    assert_eq!(
        day(&e, &db, 18),
        vec![(id, Some("Standup".into()), Some(key(18)))]
    );

    // A second edit of the same occurrence keeps what the first set.
    e.apply(
        &mut db,
        scoped_update(id, EditScope::This { key: key(11) }, retitle("Sync")),
    )
    .unwrap();
    let stored = read_block(db.conn(), id.bytes()).unwrap().unwrap();
    assert_eq!(stored.exceptions.len(), 1);
    assert_eq!(stored.exceptions[&key(11)].title.as_deref(), Some("Sync"));
    assert_eq!(stored.exceptions[&key(11)].starts_at, Some(at(11, 10)));
    assert_eq!(stored.title.as_deref(), Some("Standup"));
    e.assert_rebuild_identity(&db);
}

/// What one occurrence cannot hold, a key the rule does not produce, and a
/// key on a block with no rule are each refused, writing nothing.
#[test]
fn an_occurrence_edit_the_series_cannot_take_is_refused() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let once = e
        .apply(
            &mut db,
            Command::CreateBlock(block_draft(14, 1, Some("Gym"))),
        )
        .unwrap()
        .entity;
    let ops = op_count(&db);
    let this = |k: String| EditScope::This { key: k };

    let restream = BlockPatch {
        stream_id: Some(None),
        ..BlockPatch::default()
    };
    invalid(
        e.apply(&mut db, scoped_update(id, this(key(11)), restream)),
        "an edit of one occurrence",
    );
    let clear = BlockPatch {
        title: Some(None),
        ..BlockPatch::default()
    };
    invalid(
        e.apply(&mut db, scoped_update(id, this(key(11)), clear)),
        "an edit of one occurrence",
    );
    assert!(matches!(
        e.apply(
            &mut db,
            scoped_update(id, this("2026-03-12T09:00".into()), retitle("x"))
        ),
        Err(EngineError::NotFound(_))
    ));
    invalid(
        e.apply(
            &mut db,
            scoped_update(id, this("soon".into()), retitle("x")),
        ),
        "not an occurrence key",
    );
    invalid(
        e.apply(&mut db, scoped_update(once, this(key(4)), retitle("x"))),
        "does not recur",
    );
    invalid(
        e.apply(
            &mut db,
            Command::DeleteBlock {
                id: once,
                scope: EditScope::ThisAndFuture { key: key(4) },
            },
        ),
        "does not recur",
    );
    let inverted = BlockPatch {
        ends_at: Some(at(11, 8)),
        ..BlockPatch::default()
    };
    assert!(matches!(
        e.apply(&mut db, scoped_update(id, this(key(11)), inverted)),
        Err(EngineError::Validation(ValidationError::Field {
            field: "block.ends_at",
            constraint: "after_starts_at",
        }))
    ));
    assert_eq!(op_count(&db), ops, "a refused edit wrote nothing");
}

/// Deleting *this* cancels the one occurrence, and the block stays.
#[test]
fn deleting_this_occurrence_cancels_that_week_only() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    e.apply(
        &mut db,
        Command::DeleteBlock {
            id,
            scope: EditScope::This { key: key(11) },
        },
    )
    .unwrap();
    assert!(day(&e, &db, 11).is_empty());
    assert_eq!(day(&e, &db, 18).len(), 1);
    let stored = read_block(db.conn(), id.bytes()).unwrap().unwrap();
    assert!(!stored.deleted);
    assert!(stored.exceptions[&key(11)].cancelled);

    // A cancelled occurrence is not there to edit.
    assert!(matches!(
        e.apply(
            &mut db,
            scoped_update(id, EditScope::This { key: key(11) }, retitle("x"))
        ),
        Err(EngineError::NotFound(_))
    ));
}

/// *This and future* ends the series before the key and continues it in the
/// block `split_block_id(root, key)` names, with the edit applied, as one
/// group. Splitting again at that key edits the continuation.
#[test]
fn an_edit_of_this_and_future_splits_the_series_at_the_key() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let ops = op_count(&db);
    let next = e
        .apply(
            &mut db,
            scoped_update(
                id,
                EditScope::ThisAndFuture { key: key(18) },
                retitle("Retro"),
            ),
        )
        .unwrap()
        .entity;
    assert_eq!(next, split_block_id(&id, &key(18)));
    assert_eq!(
        op_count(&db),
        ops + 2,
        "the ended series and its continuation"
    );

    assert_eq!(
        day(&e, &db, 11),
        vec![(id, Some("Standup".into()), Some(key(11)))]
    );
    assert_eq!(
        day(&e, &db, 18),
        vec![(next, Some("Retro".into()), Some(key(18)))]
    );
    assert_eq!(
        day(&e, &db, 25),
        vec![(next, Some("Retro".into()), Some(key(25)))]
    );

    let continuation = read_block(db.conn(), next.bytes()).unwrap().unwrap();
    assert_eq!(continuation.split_from, Some(id));
    assert_eq!(continuation.starts_at, at(18, 9));
    let ended = read_block(db.conn(), id.bytes()).unwrap().unwrap();
    assert_eq!(ended.rrule.unwrap().until, Some(at(11, 9)));

    // Again at the same key: the continuation is edited, nothing new made.
    let again = e
        .apply(
            &mut db,
            scoped_update(
                id,
                EditScope::ThisAndFuture { key: key(18) },
                retitle("Review"),
            ),
        )
        .unwrap()
        .entity;
    assert_eq!(again, next);
    assert_eq!(
        day(&e, &db, 25),
        vec![(next, Some("Review".into()), Some(key(25)))]
    );
    e.assert_rebuild_identity(&db);
}

/// At the series' first occurrence *this and future* is the whole series.
#[test]
fn an_edit_of_this_and_future_at_the_first_occurrence_edits_the_block() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let res = e
        .apply(
            &mut db,
            scoped_update(
                id,
                EditScope::ThisAndFuture { key: key(4) },
                retitle("Retro"),
            ),
        )
        .unwrap();
    assert_eq!(res.entity, id);
    assert_eq!(
        day(&e, &db, 4),
        vec![(id, Some("Retro".into()), Some(key(4)))]
    );
    assert_eq!(
        day(&e, &db, 25),
        vec![(id, Some("Retro".into()), Some(key(25)))]
    );
}

/// Deleting *this and future* ends the series before the key; at the first
/// occurrence it tombstones the block.
#[test]
fn deleting_this_and_future_ends_the_series() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    e.apply(
        &mut db,
        Command::DeleteBlock {
            id,
            scope: EditScope::ThisAndFuture { key: key(18) },
        },
    )
    .unwrap();
    assert_eq!(day(&e, &db, 11).len(), 1);
    assert!(day(&e, &db, 18).is_empty());
    assert!(day(&e, &db, 25).is_empty());
    assert!(read_block(db.conn(), split_block_id(&id, &key(18)).bytes())
        .unwrap()
        .is_none());

    e.apply(
        &mut db,
        Command::DeleteBlock {
            id,
            scope: EditScope::ThisAndFuture { key: key(4) },
        },
    )
    .unwrap();
    assert!(read_block(db.conn(), id.bytes()).unwrap().unwrap().deleted);
    assert!(day(&e, &db, 4).is_empty());
}

/// Two devices that split one series at the same occurrence derive the same
/// continuation id, so after they sync there is one continuation, its
/// fields merged, and the week shows one standup.
#[test]
fn two_devices_splitting_at_the_same_key_converge_on_one_block() {
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
    set_clock(&ca, T0 + 1_000);
    recur(&ea, &mut dba, id, "FREQ=WEEKLY");
    sync(&mut dba, &mut dbb);

    let split = |k: &str| EditScope::ThisAndFuture { key: k.to_owned() };
    set_clock(&ca, T0 + 2_000);
    let on_a = ea
        .apply(
            &mut dba,
            scoped_update(id, split(&key(18)), retitle("Retro")),
        )
        .unwrap()
        .entity;
    set_clock(&cb, T0 + 3_000);
    let on_b = eb
        .apply(
            &mut dbb,
            scoped_update(id, split(&key(18)), retitle("Review")),
        )
        .unwrap()
        .entity;
    assert_eq!(on_a, on_b);
    sync(&mut dba, &mut dbb);
    sync(&mut dba, &mut dbb);

    for (e, db) in [(&ea, &dba), (&eb, &dbb)] {
        assert_eq!(
            day(e, db, 25),
            vec![(on_a, Some("Review".into()), Some(key(25)))],
            "the later title wins, on one block"
        );
        assert_eq!(day(e, db, 11).len(), 1);
        let continuations: i64 = db
            .conn()
            .query_row(
                "SELECT count(*) FROM blocks WHERE split_from = ? AND deleted = 0",
                [&id.bytes()[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(continuations, 1);
    }
    assert_eq!(
        read_block(dba.conn(), on_a.bytes()).unwrap(),
        read_block(dbb.conn(), on_a.bytes()).unwrap()
    );
    assert_eq!(
        read_block(dba.conn(), id.bytes()).unwrap(),
        read_block(dbb.conn(), id.bytes()).unwrap()
    );
    ea.assert_rebuild_identity(&dba);
    eb.assert_rebuild_identity(&dbb);
}

/// Give block `id` an import's `source` and `external_id`, as an importer
/// that maps them would (#1149).
fn imported_from(e: &Engine, db: &mut Db, id: EntityRef) {
    let b = Block {
        source: Some(sunrise_domain::BlockSource::ImportIcs),
        external_id: Some("standup@example.com".into()),
        ..read_block(db.conn(), id.bytes()).unwrap().unwrap()
    };
    let stream = crate::engine::ids::domain_bytes(b.stream_id);
    let now_ms = e.clock.now_ms();
    let op = InnerOp::BlockUpdate(Box::new(b));
    db.with_tx(|tx| e.append_and_fold(tx, &op, &stream, now_ms).map(|_| ()))
        .unwrap();
}

/// A split leaves the task bindings and the import fields on the block they
/// were made on: the continuation binds no task and is no external item's,
/// and it keeps the title the series showed (time-blocks.md §Recurring
/// blocks).
#[test]
fn a_split_continuation_takes_no_bindings_and_no_import_fields() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let task = new_task(&e, &mut db, "Prepare notes");
    e.apply(&mut db, Command::BindTask { block: id, task })
        .unwrap();
    imported_from(&e, &mut db, id);

    let move_later = BlockPatch {
        starts_at: Some(at(18, 10)),
        ends_at: Some(at(18, 11)),
        ..BlockPatch::default()
    };
    let next = e
        .apply(
            &mut db,
            scoped_update(id, EditScope::ThisAndFuture { key: key(18) }, move_later),
        )
        .unwrap()
        .entity;

    let continuation = read_block(db.conn(), next.bytes()).unwrap().unwrap();
    assert!(continuation.tasks.is_empty(), "no binding is copied");
    assert_eq!(continuation.source, None);
    assert_eq!(continuation.external_id, None);
    assert_eq!(continuation.title.as_deref(), Some("Standup"));
    assert!(!continuation.title_track_task);

    let ended = read_block(db.conn(), id.bytes()).unwrap().unwrap();
    assert_eq!(ended.tasks, BTreeSet::from([task]), "the original keeps it");
    assert_eq!(ended.source, Some(sunrise_domain::BlockSource::ImportIcs));
    assert_eq!(ended.external_id.as_deref(), Some("standup@example.com"));

    // The task lists the original block only, and only its rows bind it.
    let bound = read_task(db.conn(), task.bytes()).unwrap().unwrap();
    assert_eq!(bound.blocks, BTreeSet::from([id]));
    let week_of = |d: i8| day_rows(&e, &db, march(d)).remove(0);
    assert_eq!(week_of(11).task_titles, vec!["Prepare notes".to_string()]);
    assert!(week_of(25).task_titles.is_empty());
    e.assert_rebuild_identity(&db);
}

/// A split continuation of an untitled block bound to one task shows the
/// task's title, as the series did, without tracking it.
#[test]
fn a_split_continuation_keeps_the_title_a_bound_task_lent_the_series() {
    let mut db = db();
    let e = engine();
    legacy_peer(&db);
    let task = new_task(&e, &mut db, "Write the report");
    let id = e
        .apply(
            &mut db,
            Command::CreateBlock(BlockDraft {
                title_track_task: true,
                tasks: vec![task],
                ..block_draft(9, 1, None)
            }),
        )
        .unwrap()
        .entity;
    e.require_features(&mut db, &[BLOCK_RECURRENCE_FEATURE], true)
        .unwrap();
    recur(&e, &mut db, id, "FREQ=WEEKLY");
    let next = e
        .apply(
            &mut db,
            scoped_update(
                id,
                EditScope::ThisAndFuture { key: key(18) },
                BlockPatch {
                    ends_at: Some(at(18, 11)),
                    ..BlockPatch::default()
                },
            ),
        )
        .unwrap()
        .entity;
    assert_eq!(
        day(&e, &db, 25),
        vec![(next, Some("Write the report".into()), Some(key(25)))]
    );
    let continuation = read_block(db.conn(), next.bytes()).unwrap().unwrap();
    assert!(continuation.tasks.is_empty());
    assert!(!continuation.title_track_task);
}

/// A split names what it wrote: the continuation made and the series
/// ended. A continuation tombstoned before is written back, an update.
#[test]
fn a_split_names_the_continuation_it_made_and_the_block_it_ended() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let split = |db: &mut Db, title: &str| {
        e.apply(
            db,
            scoped_update(
                id,
                EditScope::ThisAndFuture { key: key(18) },
                retitle(title),
            ),
        )
        .unwrap()
    };
    let res = split(&mut db, "Retro");
    let next = split_block_id(&id, &key(18));
    assert_eq!(
        res.events,
        vec![DomainEvent::Created(next), DomainEvent::Updated(id)]
    );

    e.apply(
        &mut db,
        Command::DeleteBlock {
            id: next,
            scope: EditScope::All,
        },
    )
    .unwrap();
    // A concurrent edit of the whole series that won its rule back, so the
    // key is an occurrence of it again.
    recur(&e, &mut db, id, "FREQ=WEEKLY");
    let res = split(&mut db, "Review");
    assert_eq!(
        res.events,
        vec![DomainEvent::Updated(next), DomainEvent::Updated(id)]
    );
    assert!(
        !read_block(db.conn(), next.bytes())
            .unwrap()
            .unwrap()
            .deleted
    );
}

/// A *this and future* delete names the block deleted when it tombstoned
/// it (at the first occurrence) and leaves the default, an update, when it
/// only ended the series. An edit that is one of the whole block names
/// nothing of its own.
#[test]
fn a_this_and_future_delete_names_the_tombstone_it_wrote() {
    let mut db = db();
    let e = engine();
    let id = weekly(&e, &mut db);
    let end = |db: &mut Db, k: String| {
        e.apply(
            db,
            Command::DeleteBlock {
                id,
                scope: EditScope::ThisAndFuture { key: k },
            },
        )
        .unwrap()
    };
    let res = end(&mut db, key(18));
    assert!(res.events.is_empty());
    assert!(!read_block(db.conn(), id.bytes()).unwrap().unwrap().deleted);

    let res = e
        .apply(
            &mut db,
            scoped_update(id, EditScope::ThisAndFuture { key: key(4) }, retitle("x")),
        )
        .unwrap();
    assert!(res.events.is_empty());

    let res = end(&mut db, key(4));
    assert_eq!(res.events, vec![DomainEvent::Deleted(id)]);
    assert!(read_block(db.conn(), id.bytes()).unwrap().unwrap().deleted);
}

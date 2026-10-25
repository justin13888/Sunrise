//! The editors, the defer and the merge against a real vault: what a client
//! submits from these functions is what the vault then holds, kind and all.

use std::sync::Arc;

use jiff::civil::date;
use sunrise_client_core::calendar::{merge_blocks, new_block_draft, NEW_BLOCK_KIND};
use sunrise_client_core::civil::reader_zone;
use sunrise_client_core::editor::{task_form, task_patch, TaskFields};
use sunrise_client_core::schedule::defer_command;
use sunrise_core::{Clock, Command, Core, CoreConfig, Query, QueryResult, SystemRng, Unlock};
use sunrise_crypto::keys::VaultRootKey;
use sunrise_domain::{SunriseTime, Task, TaskDraft};
use sunrise_id::EntityRef;

/// 2026-01-01T00:00:00Z in UTC, so nothing here reads the host clock or zone.
const NOW_MS: u64 = 1_767_225_600_000;

#[derive(Debug)]
struct Fixed;

impl Clock for Fixed {
    fn now_ms(&self) -> u64 {
        NOW_MS
    }
    fn timezone(&self) -> String {
        "UTC".to_owned()
    }
}

async fn open(dir: &std::path::Path) -> Core {
    let cfg = CoreConfig::with_clock(
        dir.to_path_buf(),
        "0.1.0+test",
        Arc::new(Fixed),
        Arc::new(SystemRng),
    );
    Core::open(
        cfg,
        Unlock::DevicePaired {
            root: VaultRootKey::from_bytes([9; 32]),
            paired: None,
        },
    )
    .await
    .expect("open a scratch vault")
}

async fn task(core: &Core, id: EntityRef) -> Task {
    match core.query(Query::EntityById(id)).await.unwrap() {
        QueryResult::Task(t) => *t,
        other => panic!("{other:?}"),
    }
}

/// #515 end to end: a title edit from the task editor leaves an all-day
/// deadline an all-day deadline.
#[tokio::test]
async fn a_title_edit_keeps_an_all_day_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    let due = SunriseTime::all_day(date(2026, 1, 9));
    let id = core
        .submit(Command::CreateTask(TaskDraft {
            title: "Renew passport".into(),
            due_at: Some(due.clone()),
            ..TaskDraft::default()
        }))
        .await
        .unwrap()
        .entity;

    let tz = reader_zone("UTC").unwrap();
    let stored = TaskFields::from(&task(&core, id).await);
    let mut form = task_form(&stored, &tz);
    form.title = "Renew passport this week".into();
    let patch = task_patch(&stored, &form, &tz).unwrap();
    core.submit(Command::UpdateTask { id, patch })
        .await
        .unwrap();

    let after = task(&core, id).await;
    assert_eq!(after.title, "Renew passport this week");
    assert_eq!(after.due_at, Some(due));
}

#[tokio::test]
async fn a_defer_keeps_a_floating_plan_floating_and_counts() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    let plan = SunriseTime::floating(date(2026, 1, 1).at(9, 0, 0, 0));
    let id = core
        .submit(Command::CreateTask(TaskDraft {
            title: "Call the plumber".into(),
            scheduled_at: Some(plan.clone()),
            ..TaskDraft::default()
        }))
        .await
        .unwrap()
        .entity;

    let now = jiff::Timestamp::from_millisecond(i64::try_from(NOW_MS).unwrap()).unwrap();
    let tz = reader_zone("UTC").unwrap();
    core.submit(defer_command(id, Some(&plan), 1, now, &tz).unwrap())
        .await
        .unwrap();

    let after = task(&core, id).await;
    assert_eq!(
        after.scheduled_at,
        Some(SunriseTime::floating(date(2026, 1, 2).at(9, 0, 0, 0)))
    );
    assert_eq!(after.deferred_count, 1);
}

/// The Resolve menu's Merge, in one call: the union stands and both
/// originals are gone.
#[tokio::test]
async fn a_merge_leaves_only_the_union() {
    let dir = tempfile::tempdir().unwrap();
    let core = open(dir.path()).await;
    let tz = reader_zone("UTC").unwrap();
    let hour = 3_600_000;
    let nine = i64::try_from(NOW_MS).unwrap() + 9 * hour;
    let mut ids = Vec::new();
    for (from, to, title) in [
        (nine, nine + 2 * hour, "Deep work"),
        (nine + hour, nine + 4 * hour, "Review"),
    ] {
        let draft = new_block_draft(
            from,
            to,
            NEW_BLOCK_KIND,
            None,
            Some(title.into()),
            vec![],
            &tz,
        )
        .unwrap();
        ids.push(
            core.submit(Command::CreateBlock(draft))
                .await
                .unwrap()
                .entity,
        );
    }

    let merged = merge_blocks(&core, ids[0], ids[1], &tz).await.unwrap();

    let day = u64::try_from(nine).unwrap();
    let QueryResult::Blocks(rows) = core.query(Query::DayBlocks { day_ms: day }).await.unwrap()
    else {
        panic!("expected blocks");
    };
    assert_eq!(
        rows.iter().map(|r| r.block.id).collect::<Vec<_>>(),
        [merged]
    );
    let block = &rows[0].block;
    assert_eq!(
        block.starts_at,
        SunriseTime::zoned(date(2026, 1, 1).at(9, 0, 0, 0), "UTC")
    );
    assert_eq!(
        block.ends_at,
        SunriseTime::zoned(date(2026, 1, 1).at(13, 0, 0, 0), "UTC")
    );
}

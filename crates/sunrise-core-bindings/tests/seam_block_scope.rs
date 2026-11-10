//! A block's edit scope across the seam (issue #1148): `BlockEdit.scope` and
//! `CoreCommand::DeleteBlock.scope` reach the core's scoped commands, and a
//! one-off block's grid row carries no occurrence key.
//!
//! No command creates a recurring block yet, so an occurrence scope is driven
//! at a one-off block here, where the core refuses it as not recurring: the
//! refusal is what shows the scope crossed, since with no scope the same
//! command edits or tombstones the whole block. A recurring row's key is
//! covered by `From<&BlockRow> for BlockGridRow`'s unit test.
//! `flavor = "multi_thread"` for the reason `seam.rs` gives.

use sunrise_core_bindings::dto::{BlockDraftIn, BlockEdit, BlockGridRow, EditScopeIn, TimeValue};
use sunrise_core_bindings::vocab::{inbox_stream_id, time_value_ms};
use sunrise_core_bindings::{CoreCommand, CoreQuery, CoreQueryResult, SunriseCore};
use sunrise_id::EntityRef;

mod support;
use support::{draft, open_core};

fn floating(civil: &str) -> TimeValue {
    TimeValue::Floating {
        civil: civil.parse().expect("civil"),
    }
}

async fn day(core: &SunriseCore) -> Vec<BlockGridRow> {
    let day_ms =
        u64::try_from(time_value_ms(floating("2026-03-04T12:00:00"), "UTC".into())).expect("day");
    match core
        .query(CoreQuery::DayBlocks { day_ms })
        .await
        .expect("day grid")
    {
        CoreQueryResult::Blocks { blocks } => blocks,
        other => panic!("expected blocks, got {other:?}"),
    }
}

async fn standup(core: &SunriseCore, task: EntityRef) -> EntityRef {
    core.submit(CoreCommand::CreateBlock {
        draft: BlockDraftIn {
            stream_id: inbox_stream_id(),
            starts_at: floating("2026-03-04T09:00:00"),
            ends_at: floating("2026-03-04T10:00:00"),
            title: Some("Standup".into()),
            title_track_task: false,
            tasks: vec![task],
        },
    })
    .await
    .expect("create block")
    .entity
}

fn retitle(title: &str, scope: Option<EditScopeIn>) -> BlockEdit {
    BlockEdit {
        set_title: Some(title.into()),
        scope,
        ..BlockEdit::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocks_edit_and_delete_scope_reach_the_core() {
    let (_dir, core) = open_core().await;
    let task = core
        .submit(CoreCommand::CreateTask {
            draft: draft("Prepare notes"),
        })
        .await
        .expect("create task")
        .entity;
    let id = standup(&core, task).await;
    let rows = day(&core).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].occurrence_key, None, "a one-off block has no key");
    assert_eq!(rows[0].task_titles, ["Prepare notes"]);

    let key = "2026-03-04T09:00".to_owned();
    let scopes = [
        EditScopeIn::This { key: key.clone() },
        EditScopeIn::ThisAndFuture { key },
    ];
    for scope in scopes {
        let err = core
            .submit(CoreCommand::UpdateBlock {
                id,
                edit: retitle("Retro", Some(scope.clone())),
            })
            .await
            .expect_err("an occurrence of a one-off block");
        assert!(format!("{err:?}").contains("does not recur"), "{err:?}");
        let err = core
            .submit(CoreCommand::DeleteBlock {
                id,
                scope: Some(scope),
            })
            .await
            .expect_err("an occurrence of a one-off block");
        assert!(format!("{err:?}").contains("does not recur"), "{err:?}");
    }
    let rows = day(&core).await;
    assert_eq!(rows.len(), 1, "a refused scope wrote nothing");
    assert_eq!(rows[0].title.as_deref(), Some("Standup"));
    assert_eq!(rows[0].task_titles, ["Prepare notes"]);

    // `All` is the whole block.
    core.submit(CoreCommand::UpdateBlock {
        id,
        edit: retitle("Retro", Some(EditScopeIn::All)),
    })
    .await
    .expect("edit the whole block");
    assert_eq!(day(&core).await[0].title.as_deref(), Some("Retro"));
    core.submit(CoreCommand::DeleteBlock {
        id,
        scope: Some(EditScopeIn::All),
    })
    .await
    .expect("tombstone the block");
    assert!(day(&core).await.is_empty());
    core.shutdown().await;
}

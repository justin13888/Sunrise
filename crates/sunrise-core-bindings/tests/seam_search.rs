//! Search across the seam (issue #345): the grouped query, its highlight
//! ranges and notice codes, and the content-language re-index, against a real
//! vault. `flavor = "multi_thread"` for the reason `seam.rs` gives.

use sunrise_core_bindings::search::{HighlightRange, SearchKindItem};
use sunrise_core_bindings::{CoreCommand, CoreQuery, CoreQueryResult};

mod support;
use support::{draft, open_core};

/// Grouped search crosses the seam with its groups, highlight ranges in
/// Unicode scalars, notice codes and the canonical query, and the re-index
/// of a vault whose language never changed has nothing to do.
#[tokio::test(flavor = "multi_thread")]
async fn grouped_search_crosses_the_seam_with_highlights_and_notices() {
    let (_dir, core) = open_core().await;
    let task = core
        .submit(CoreCommand::CreateTask {
            draft: draft("Renew the passport in 東京"),
        })
        .await
        .expect("create")
        .entity;
    let CoreQueryResult::Search { results } = core
        .query(CoreQuery::SearchGrouped {
            text: "東京 pass colour:red".into(),
            now_ms: core.now_ms(),
            prefix: false,
            per_group: None,
        })
        .await
        .expect("query")
    else {
        panic!("SearchGrouped answered another variant");
    };
    assert!(
        results.groups.is_empty(),
        "colour:red is a word no task holds"
    );
    assert_eq!(results.notices[0].code, "unknown_key");
    assert_eq!(results.canonical, "東京 pass colour:red");
    let CoreQueryResult::Search { results } = core
        .query(CoreQuery::SearchGrouped {
            text: "東京 pass".into(),
            now_ms: core.now_ms(),
            prefix: false,
            per_group: None,
        })
        .await
        .expect("query")
    else {
        panic!("SearchGrouped answered another variant");
    };
    let group = &results.groups[0];
    assert_eq!(group.kind, SearchKindItem::Task);
    assert_eq!(group.total, 1);
    let hit = &group.hits[0];
    assert_eq!((hit.entity, hit.opens), (task, task));
    assert_eq!(
        hit.title.ranges,
        vec![
            HighlightRange { start: 10, end: 14 },
            HighlightRange { start: 22, end: 24 },
        ]
    );
    let step = core.reindex_search().await.expect("reindex");
    assert!(step.finished);
    core.shutdown().await;
}

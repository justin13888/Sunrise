//! Search across the seam (issue #345): the grouped query, its highlight
//! ranges and notice codes, and the content-language re-index, against a real
//! vault. `flavor = "multi_thread"` for the reason `seam.rs` gives.

use sunrise_core_bindings::search::{
    search_completions, HighlightRange, SearchCompletionKindItem, SearchKindItem,
};
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

/// The field's autocomplete crosses the seam as whole-field suggestions,
/// each saying what it is so a client can describe it.
#[test]
fn completions_cross_the_seam_with_what_each_one_is() {
    let keys = search_completions("deploy d".into(), vec![], vec![]);
    assert_eq!(
        keys.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
        ["deploy due:", "deploy done:"]
    );
    assert_eq!(keys[0].kind, SearchCompletionKindItem::Key);
    assert_eq!(keys[0].key, "due");
    assert_eq!(keys[0].value, None);

    let values = search_completions("is:la".into(), vec![], vec![]);
    assert_eq!(values[0].label, "late");
    assert_eq!(values[0].kind, SearchCompletionKindItem::Value);
    assert_eq!(
        (values[0].key.as_str(), values[0].value.as_deref()),
        ("is", Some("late"))
    );

    let dates = search_completions("due<tom".into(), vec![], vec![]);
    assert_eq!(dates[0].key, "date");

    let names = search_completions(
        "#si".into(),
        vec!["Side project".into()],
        vec!["errands".into()],
    );
    assert_eq!(names[0].text, "#\"Side project\" ");
    assert_eq!(
        (names[0].kind, names[0].key.as_str()),
        (SearchCompletionKindItem::Stream, "stream")
    );
    let contexts = search_completions("@".into(), vec![], vec!["errands".into()]);
    assert_eq!(contexts[0].kind, SearchCompletionKindItem::Context);
}

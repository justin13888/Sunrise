//! The views file's round trip: whatever [`SavedViewStore::save`] accepts,
//! [`SavedViewStore::load`] reads back as the same set, with no warnings.
//!
//! The file's subset cannot carry every string (`views.rs`'s `parse_view` and
//! `parse_pairs`), so `save` refuses a view it could not read back rather
//! than writing one the next `load` drops. A view of plain text is always
//! accepted, so the property is not satisfied by refusing everything.

use proptest::prelude::*;
use sunrise_client_core::views::{SaveError, SavedView, SavedViewStore, View};

const VIEWS: [View; 10] = [
    View::Today,
    View::Inbox,
    View::Stream,
    View::Search,
    View::Calendar,
    View::Focus,
    View::Routines,
    View::Review,
    View::Morning,
    View::Evening,
];

/// Text over the characters the file's grammar gives meaning to, and some it
/// does not.
fn text() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        proptest::sample::select(vec![
            "a", "b", "é", "東", " ", "\t", "=", ";", ",", "\"", "'", "#", "@", "[", "]", "\n",
            "\r", "view=", "query=",
        ]),
        0..8,
    )
    .prop_map(|parts| parts.concat())
}

/// Text the subset always carries.
fn plain() -> impl Strategy<Value = String> {
    "[a-z東é][a-z東é -]{0,6}[a-z東é]|[a-z東é]"
}

fn view_with(
    name: impl Strategy<Value = String>,
    text: fn() -> BoxedStrategy<String>,
) -> impl Strategy<Value = SavedView> {
    (
        name,
        proptest::sample::select(VIEWS.to_vec()),
        prop_oneof![Just(String::new()), text()],
        proptest::collection::vec(text(), 0..3),
    )
        .prop_map(|(name, view, query, contexts)| SavedView {
            name,
            view,
            query,
            contexts,
        })
}

fn any_view() -> impl Strategy<Value = SavedView> {
    view_with(text(), || text().boxed())
}

fn plain_view() -> impl Strategy<Value = SavedView> {
    view_with(plain(), || plain().boxed())
}

fn store() -> (tempfile::TempDir, SavedViewStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = SavedViewStore::at(Some(dir.path().join("views.toml")));
    (dir, store)
}

proptest! {
    // `Direct`, not the `SourceParallel` default: see
    // docs/10-cross-cutting/testing.md section 2.
    #![proptest_config(ProptestConfig {
        cases: 1024,
        rng_seed: sunrise_test_seed::proptest_rng_seed(),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::Direct(
                "proptest-regressions/tests/saved_views_proptest.txt",
            ),
        )),
        ..ProptestConfig::default()
    })]

    /// `load(save(v)) == v` for every set `save` accepts.
    #[test]
    fn an_accepted_set_reads_back_as_itself(views in proptest::collection::vec(any_view(), 0..4)) {
        let (_dir, store) = store();
        if store.save(&views).is_ok() {
            let (read, warnings) = store.load();
            prop_assert!(warnings.is_empty(), "{warnings:?}");
            prop_assert_eq!(read, views);
        }
    }

    /// A refused set writes nothing: the file keeps the set it held.
    #[test]
    fn a_refused_set_leaves_the_file_as_it_was(views in proptest::collection::vec(any_view(), 1..4)) {
        let (_dir, store) = store();
        let before = vec![SavedView {
            name: "kept".into(),
            view: View::Today,
            query: String::new(),
            contexts: Vec::new(),
        }];
        store.save(&before).unwrap();
        if store.save(&views).is_err() {
            prop_assert_eq!(store.load().0, before);
        }
    }

    /// Plain text is never refused.
    #[test]
    fn a_plain_set_is_accepted(views in proptest::collection::vec(plain_view(), 0..4)) {
        let (_dir, store) = store();
        prop_assert!(store.save(&views).is_ok());
        prop_assert_eq!(store.load().0, views);
    }
}

/// The three shapes the file used to write and then refuse, refused at
/// `save` with the store's error.
#[test]
fn a_semicolon_query_or_an_equals_or_quote_name_is_refused() {
    let (_dir, store) = store();
    for (name, query) in [
        ("errands", "milk;eggs"),
        ("a=b", "milk"),
        ("say \"hi\"", "milk"),
    ] {
        let v = SavedView {
            name: name.into(),
            view: View::Search,
            query: query.into(),
            contexts: Vec::new(),
        };
        let err = store.save(&[v]).unwrap_err();
        assert!(
            matches!(&err, SaveError::Unencodable { name: n, .. } if n == name),
            "{name:?}: {err:?}"
        );
    }
}

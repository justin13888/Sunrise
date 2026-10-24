//! The client layer's half of the synced `SavedView` entity (#341): the
//! one-time import of a device's `views.toml`, and the words a client shows a
//! view in. See the [parent module](super).

use super::{parse_view_name, SavedView, View};
use sunrise_domain::{
    resolve_named, NameKind, NamedRef, SavedView as EntityView, SavedViewDraft, ViewFilter,
    ViewKind, ViewSort,
};
use sunrise_id::EntityRef;

/// The file, in a vault's directory, whose presence says this device's
/// `views.toml` was imported into that vault. The views file itself is left
/// where it is: until the Apple client's own import ships, it still reads it.
pub const IMPORT_MARKER: &str = "views.toml.imported";

/// The entity's view for the file's.
#[must_use]
pub const fn view_kind(v: View) -> ViewKind {
    match v {
        View::Today => ViewKind::Today,
        View::Inbox => ViewKind::Inbox,
        View::Stream => ViewKind::Stream,
        View::Search => ViewKind::Search,
        View::Calendar => ViewKind::Calendar,
        View::Focus => ViewKind::Focus,
        View::Routines => ViewKind::Routines,
        View::Review => ViewKind::Review,
        View::Morning => ViewKind::Morning,
        View::Evening => ViewKind::Evening,
    }
}

/// The primary view an entity's view opens, or `None` for one a newer build
/// added.
#[must_use]
pub fn primary_view(k: &ViewKind) -> Option<View> {
    Some(match k {
        ViewKind::Today => View::Today,
        ViewKind::Inbox => View::Inbox,
        ViewKind::Stream => View::Stream,
        ViewKind::Search => View::Search,
        ViewKind::Calendar => View::Calendar,
        ViewKind::Focus => View::Focus,
        ViewKind::Routines => View::Routines,
        ViewKind::Review => View::Review,
        ViewKind::Morning => View::Morning,
        ViewKind::Evening => View::Evening,
        ViewKind::Unknown(_) => return None,
    })
}

/// A view or sort spelling a user typed, or `None` for one no build has.
/// The file's older spellings (`browse`, `eod`, …) are accepted too.
#[must_use]
pub fn parse_view_kind(s: &str) -> Option<ViewKind> {
    parse_view_name(s.trim()).map(view_kind)
}

/// One-line summary of an entity's view, with each context named by
/// `context_name` (its current name, so a rename shows at once).
#[must_use]
pub fn describe_entity(
    v: &EntityView,
    context_name: &dyn Fn(EntityRef) -> Option<String>,
) -> String {
    let mut parts = vec![v.view.as_str().to_owned()];
    let f = &v.filter;
    if !f.text.is_empty() {
        parts.push(format!("/{}", f.text));
    }
    let mut ctx: Vec<String> = f
        .contexts
        .iter()
        .map(|c| format!("@{}", context_name(*c).unwrap_or_else(|| c.to_str())))
        .collect();
    ctx.extend(f.context_names.iter().map(|n| format!("@{n}?")));
    if !ctx.is_empty() {
        parts.push(ctx.join(" "));
    }
    if let Some(p) = &f.priority {
        parts.push(format!("!{}-{}", p.min, p.max));
    }
    if !v.sort.is_unknown() && v.sort != ViewSort::Default {
        parts.push(format!("by {}", v.sort.as_str()));
    }
    parts.join(" · ")
}

/// What importing a `views.toml` into a vault comes to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportPlan {
    /// One draft per view to create, in file order.
    pub drafts: Vec<SavedViewDraft>,
    /// Views not imported because the vault already has a live view of that
    /// name: imported from another device, or saved since.
    pub skipped: Vec<String>,
    /// `(view, name)` for every context or stream name that resolved to
    /// nothing. The name is kept on the view
    /// ([`sunrise_domain::ViewFilter::context_names`]) rather than dropped,
    /// and every read of the view reports it.
    pub unresolved: Vec<(String, String)>,
}

/// Plan the one-time import of `file` (the views a device's `views.toml`
/// holds) into a vault with `contexts` and `streams` (id and name) and the
/// live views `existing`.
///
/// Context names resolve as a capture token does
/// ([`sunrise_domain::resolve_named`], exact and then unique prefix). A
/// Stream view's `query` named its stream, and resolves the same way; every
/// other view's `query` is its free text.
#[must_use]
pub fn plan_import(
    file: &[SavedView],
    contexts: &[(EntityRef, String)],
    streams: &[(EntityRef, String)],
    existing: &[EntityView],
) -> ImportPlan {
    let named = |rows: &[(EntityRef, String)]| -> Vec<(EntityRef, String)> { rows.to_vec() };
    let resolve = |rows: &[(EntityRef, String)], typed: &str, kind: NameKind| {
        let owned = named(rows);
        let refs: Vec<NamedRef<'_>> = owned
            .iter()
            .map(|(id, name)| NamedRef {
                id: *id,
                name: name.as_str(),
            })
            .collect();
        let mut unresolved = Vec::new();
        resolve_named(typed, &refs, &mut unresolved, kind)
    };
    let mut plan = ImportPlan::default();
    let mut taken: Vec<String> = existing.iter().map(|v| v.name.clone()).collect();
    for v in file {
        if taken.iter().any(|n| n == &v.name) {
            plan.skipped.push(v.name.clone());
            continue;
        }
        taken.push(v.name.clone());
        let mut filter = ViewFilter::default();
        for name in &v.contexts {
            match resolve(contexts, name, NameKind::Context) {
                Some(id) if !filter.contexts.contains(&id) => filter.contexts.push(id),
                Some(_) => {}
                None => {
                    plan.unresolved.push((v.name.clone(), name.clone()));
                    filter.context_names.push(name.clone());
                }
            }
        }
        if v.view == View::Stream && !v.query.trim().is_empty() {
            match resolve(streams, v.query.trim(), NameKind::Stream) {
                Some(id) => filter.streams.push(id),
                None => plan.unresolved.push((v.name.clone(), v.query.clone())),
            }
        } else {
            filter.text.clone_from(&v.query);
        }
        plan.drafts.push(SavedViewDraft {
            name: v.name.clone(),
            view: view_kind(v.view),
            filter,
            sort: ViewSort::Default,
        });
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(name: &str, view: View, query: &str, contexts: &[&str]) -> SavedView {
        SavedView {
            name: name.into(),
            view,
            query: query.into(),
            contexts: contexts.iter().map(ToString::to_string).collect(),
        }
    }

    fn eref(kind: sunrise_id::EntityKind, n: u8) -> EntityRef {
        EntityRef::new(kind, [n; 16])
    }

    /// Names resolve to ids, a name that resolves to nothing is reported and
    /// kept on the view, a Stream view's query names its stream, and a view
    /// the vault already has by name is skipped.
    #[test]
    fn the_import_resolves_names_to_ids_and_reports_the_rest() {
        use sunrise_id::EntityKind;
        let errands = eref(EntityKind::Context, 1);
        let work = eref(EntityKind::Stream, 2);
        let contexts = vec![(errands, "errands".to_owned())];
        let streams = vec![(work, "Work".to_owned())];
        let file = vec![
            saved("run", View::Today, "", &["errands", "ghost"]),
            saved("job", View::Stream, "work", &[]),
            saved("find", View::Search, "passport", &[]),
            saved("kept", View::Inbox, "", &[]),
        ];
        let existing = vec![EntityView {
            id: eref(EntityKind::SavedView, 9),
            created_at: jiff::Timestamp::UNIX_EPOCH,
            updated_at: jiff::Timestamp::UNIX_EPOCH,
            name: "kept".into(),
            view: ViewKind::Inbox,
            filter: ViewFilter::default(),
            sort: ViewSort::Default,
            sort_order: None,
            deleted: false,
            unknown: sunrise_domain::Unknowns::new(),
        }];
        let plan = plan_import(&file, &contexts, &streams, &existing);
        assert_eq!(plan.skipped, vec!["kept".to_owned()]);
        assert_eq!(
            plan.unresolved,
            vec![("run".to_owned(), "ghost".to_owned())]
        );
        let names: Vec<&str> = plan.drafts.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["run", "job", "find"]);
        assert_eq!(plan.drafts[0].filter.contexts, vec![errands]);
        assert_eq!(
            plan.drafts[0].filter.context_names,
            vec!["ghost".to_owned()]
        );
        assert_eq!(plan.drafts[1].view, ViewKind::Stream);
        assert_eq!(plan.drafts[1].filter.streams, vec![work]);
        assert!(plan.drafts[1].filter.text.is_empty());
        assert_eq!(plan.drafts[2].filter.text, "passport");
        // A second import of the same file into the vault it went into
        // creates nothing.
        let after: Vec<EntityView> = plan
            .drafts
            .iter()
            .map(|d| EntityView {
                name: d.name.clone(),
                ..existing[0].clone()
            })
            .chain(existing.clone())
            .collect();
        assert!(plan_import(&file, &contexts, &streams, &after)
            .drafts
            .is_empty());
    }

    #[test]
    fn every_file_view_is_an_entity_view_and_back() {
        for v in [
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
        ] {
            assert_eq!(primary_view(&view_kind(v)), Some(v));
        }
        assert_eq!(primary_view(&ViewKind::from_raw("map")), None);
        assert_eq!(parse_view_kind("eod"), Some(ViewKind::Evening));
        assert_eq!(parse_view_kind("nope"), None);
    }

    #[test]
    fn an_entity_view_is_described_by_its_contexts_current_names() {
        use sunrise_id::EntityKind;
        let c = eref(EntityKind::Context, 1);
        let v = EntityView {
            id: eref(EntityKind::SavedView, 9),
            created_at: jiff::Timestamp::UNIX_EPOCH,
            updated_at: jiff::Timestamp::UNIX_EPOCH,
            name: "x".into(),
            view: ViewKind::Search,
            filter: ViewFilter {
                contexts: vec![c],
                context_names: vec!["ghost".into()],
                text: "milk".into(),
                ..ViewFilter::default()
            },
            sort: ViewSort::Manual,
            sort_order: None,
            deleted: false,
            unknown: sunrise_domain::Unknowns::new(),
        };
        let named = |id: EntityRef| (id == c).then(|| "shops".to_owned());
        assert_eq!(
            describe_entity(&v, &named),
            "search · /milk · @shops @ghost? · by manual"
        );
    }
}

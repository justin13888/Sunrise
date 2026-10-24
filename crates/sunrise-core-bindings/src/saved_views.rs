//! Saved views, a synced entity, across the seam (#341):
//! [`sunrise_domain::SavedView`] mirrored, its draft and edit, what
//! evaluating one returns, and the one-time import of the per-device
//! `views.toml` the Apple client still reads ([`plan_saved_views_import`]).
//!
//! The view and sort are carried as their spellings, so one a newer build
//! added survives a round trip, with the primary view beside it when this
//! build knows it. The filter's typed parts are fields; whatever a newer
//! build added to it rides in `extra`, the canonical CBOR of the whole
//! filter as read, which a client hands back unchanged.

use crate::client::{PrimaryView, SavedView as FileView};
use crate::dto::TaskItem;
use sunrise_client_core::views;
use sunrise_core::SavedViewResult;
use sunrise_domain::{
    DateWindow, Energy, PriorityRange, SavedView, SavedViewDraft, SavedViewPatch, Unknowns,
    ViewFilter, ViewKind, ViewSort, WindowField,
};
use sunrise_id::EntityRef;

/// See [`sunrise_domain::ViewFilter`]. Every part is optional and they are
/// ANDed; inside a part, values are ORed.
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct ViewFilterItem {
    /// Tasks in any of these streams.
    #[uniffi(default = [])]
    pub streams: Vec<EntityRef>,
    /// Tasks carrying any of these contexts, by id.
    #[uniffi(default = [])]
    pub contexts: Vec<EntityRef>,
    /// Context names an import could not resolve; matched by nothing.
    #[uniffi(default = [])]
    pub context_names: Vec<String>,
    /// The highest priority kept (1 is highest); set with `priority_max`.
    #[uniffi(default = None)]
    pub priority_min: Option<u8>,
    /// The lowest priority kept.
    #[uniffi(default = None)]
    pub priority_max: Option<u8>,
    /// Tasks with any of these energies.
    #[uniffi(default = [])]
    pub energy: Vec<Energy>,
    /// The time a date window reads: `scheduled` or `due`, or a newer
    /// build's verbatim. No window when absent.
    #[uniffi(default = None)]
    pub window_field: Option<String>,
    /// The window's first day, in days from the reader's today.
    #[uniffi(default = None)]
    pub window_from_days: Option<i32>,
    /// The window's last day, in days from the reader's today.
    #[uniffi(default = None)]
    pub window_to_days: Option<i32>,
    /// Free text: a Search view's query, a title match otherwise.
    #[uniffi(default = "")]
    pub text: String,
    /// The filter as read, opaque; hand it back unchanged.
    #[uniffi(default = None)]
    pub extra: Option<Vec<u8>>,
}

impl From<&ViewFilter> for ViewFilterItem {
    fn from(f: &ViewFilter) -> Self {
        let whole_is_known = f.is_complete();
        Self {
            streams: f.streams.clone(),
            contexts: f.contexts.clone(),
            context_names: f.context_names.clone(),
            priority_min: f.priority.as_ref().map(|p| p.min),
            priority_max: f.priority.as_ref().map(|p| p.max),
            energy: f.energy.clone(),
            window_field: f.window.as_ref().map(|w| w.field.as_str().to_owned()),
            window_from_days: f.window.as_ref().and_then(|w| w.from_days),
            window_to_days: f.window.as_ref().and_then(|w| w.to_days),
            text: f.text.clone(),
            extra: if whole_is_known {
                None
            } else {
                sunrise_cbor::encode_canonical(f).ok()
            },
        }
    }
}

impl From<ViewFilterItem> for ViewFilter {
    fn from(i: ViewFilterItem) -> Self {
        let base: Self = i
            .extra
            .as_deref()
            .and_then(|b| sunrise_cbor::decode_lenient(b).ok())
            .unwrap_or_default();
        let priority = match (i.priority_min, i.priority_max) {
            (None, None) => None,
            (min, max) => Some(PriorityRange {
                min: min.unwrap_or(1),
                max: max.unwrap_or(5),
                unknown: base
                    .priority
                    .as_ref()
                    .map(|p| p.unknown.clone())
                    .unwrap_or_default(),
            }),
        };
        let window = i.window_field.map(|field| DateWindow {
            field: WindowField::from_raw(&field),
            from_days: i.window_from_days,
            to_days: i.window_to_days,
            unknown: base
                .window
                .as_ref()
                .map(|w| w.unknown.clone())
                .unwrap_or_default(),
        });
        Self {
            streams: i.streams,
            contexts: i.contexts,
            context_names: i.context_names,
            priority,
            energy: i.energy,
            window,
            text: i.text,
            unknown: base.unknown,
        }
    }
}

/// See [`sunrise_domain::SavedView`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SavedViewItem {
    /// Its id.
    pub id: EntityRef,
    /// Creation time.
    pub created_at: jiff::Timestamp,
    /// Last-update time.
    pub updated_at: jiff::Timestamp,
    /// The name; not unique.
    pub name: String,
    /// The view's spelling, verbatim.
    pub view: String,
    /// The primary view it opens, or none for one a newer build added.
    pub primary: Option<PrimaryView>,
    /// The filter.
    pub filter: ViewFilterItem,
    /// The order's spelling: `default`, `manual`, `created`, `updated`,
    /// `title`, `priority`, `due`, `scheduled`, or a newer one verbatim.
    pub sort: String,
    /// Its place in the saved-views list. Opaque; compare as strings.
    pub sort_order: Option<String>,
    /// Deleted.
    pub deleted: bool,
    /// The one-line summary the client layer words, contexts by id.
    pub summary: String,
}

impl From<&SavedView> for SavedViewItem {
    fn from(v: &SavedView) -> Self {
        Self {
            id: v.id,
            created_at: v.created_at,
            updated_at: v.updated_at,
            name: v.name.clone(),
            view: v.view.as_str().to_owned(),
            primary: views::primary_view(&v.view).map(PrimaryView::from),
            filter: ViewFilterItem::from(&v.filter),
            sort: v.sort.as_str().to_owned(),
            sort_order: v.sort_order.clone(),
            deleted: v.deleted,
            summary: views::describe_entity(v, &|_| None),
        }
    }
}

impl crate::dto::Mirrored for SavedView {
    type Dto = SavedViewItem;
}

/// A new saved view. See [`sunrise_domain::SavedViewDraft`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SavedViewDraftIn {
    /// The name.
    pub name: String,
    /// The view's spelling.
    pub view: String,
    /// The filter.
    #[uniffi(default = None)]
    pub filter: Option<ViewFilterItem>,
    /// The order's spelling; `default` when absent.
    #[uniffi(default = None)]
    pub sort: Option<String>,
}

impl From<SavedViewDraftIn> for SavedViewDraft {
    fn from(d: SavedViewDraftIn) -> Self {
        Self {
            name: d.name,
            view: ViewKind::from_raw(&d.view),
            filter: d.filter.map(Into::into).unwrap_or_default(),
            sort: d.sort.map_or(ViewSort::Default, |s| ViewSort::from_raw(&s)),
        }
    }
}

impl From<&SavedViewDraft> for SavedViewDraftIn {
    fn from(d: &SavedViewDraft) -> Self {
        Self {
            name: d.name.clone(),
            view: d.view.as_str().to_owned(),
            filter: Some(ViewFilterItem::from(&d.filter)),
            sort: Some(d.sort.as_str().to_owned()),
        }
    }
}

/// An edit of a saved view: each set field is written, and only those.
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct SavedViewEdit {
    /// A new name.
    #[uniffi(default = None)]
    pub name: Option<String>,
    /// A new view, by spelling.
    #[uniffi(default = None)]
    pub view: Option<String>,
    /// A new filter, whole.
    #[uniffi(default = None)]
    pub filter: Option<ViewFilterItem>,
    /// A new order, by spelling.
    #[uniffi(default = None)]
    pub sort: Option<String>,
}

impl From<SavedViewEdit> for SavedViewPatch {
    fn from(e: SavedViewEdit) -> Self {
        Self {
            name: e.name,
            view: e.view.map(|v| ViewKind::from_raw(&v)),
            filter: e.filter.map(Into::into),
            sort: e.sort.map(|s| ViewSort::from_raw(&s)),
            sort_order: None,
        }
    }
}

/// What evaluating a saved view returned. See
/// [`sunrise_core::SavedViewResult`].
#[derive(Debug, Clone, uniffi::Record)]
pub struct SavedViewResultItem {
    /// The view.
    pub view: SavedViewItem,
    /// The tasks it matches now, in its order; empty for a view of a screen
    /// with no list.
    pub tasks: Vec<TaskItem>,
    /// Contexts it names that are deleted or not held here.
    pub missing_contexts: Vec<EntityRef>,
    /// Context names an import could not resolve.
    pub unresolved_names: Vec<String>,
    /// Whether this build evaluates every part of it.
    pub complete: bool,
}

impl From<&SavedViewResult> for SavedViewResultItem {
    fn from(r: &SavedViewResult) -> Self {
        Self {
            view: SavedViewItem::from(&r.view),
            tasks: r.tasks.iter().map(TaskItem::from).collect(),
            missing_contexts: r.missing_contexts.clone(),
            unresolved_names: r.unresolved_names.clone(),
            complete: r.complete,
        }
    }
}

/// An entity and its name, for resolving the names a views file holds.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NamedEntity {
    /// The id.
    pub id: EntityRef,
    /// Its current name.
    pub name: String,
}

/// A name in the file that resolved to nothing.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct UnresolvedViewName {
    /// The view it was on.
    pub view: String,
    /// The name.
    pub name: String,
}

/// What importing a `views.toml` into a vault comes to. See
/// [`sunrise_client_core::views::ImportPlan`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SavedViewImport {
    /// Submit each as `CoreCommand::CreateSavedView`.
    pub drafts: Vec<SavedViewDraftIn>,
    /// Names the vault already has, not imported.
    pub skipped: Vec<String>,
    /// Names that resolved to nothing, to tell the user about.
    pub unresolved: Vec<UnresolvedViewName>,
}

/// Plan the one-time import of the views file's `file` views into a vault
/// whose contexts, streams and live saved views are given. The Apple client
/// calls it once per vault, submits the drafts, and stops writing the file.
#[uniffi::export]
#[must_use]
pub fn plan_saved_views_import(
    file: Vec<FileView>,
    contexts: Vec<NamedEntity>,
    streams: Vec<NamedEntity>,
    existing: Vec<SavedViewItem>,
) -> SavedViewImport {
    let file: Vec<views::SavedView> = file.into_iter().map(Into::into).collect();
    let pairs = |rows: Vec<NamedEntity>| -> Vec<(EntityRef, String)> {
        rows.into_iter().map(|r| (r.id, r.name)).collect()
    };
    let existing: Vec<SavedView> = existing
        .into_iter()
        .map(|v| SavedView {
            id: v.id,
            created_at: v.created_at,
            updated_at: v.updated_at,
            name: v.name,
            view: ViewKind::from_raw(&v.view),
            filter: v.filter.into(),
            sort: ViewSort::from_raw(&v.sort),
            sort_order: v.sort_order,
            deleted: v.deleted,
            unknown: Unknowns::new(),
        })
        .collect();
    let plan = views::plan_import(&file, &pairs(contexts), &pairs(streams), &existing);
    SavedViewImport {
        drafts: plan.drafts.iter().map(SavedViewDraftIn::from).collect(),
        skipped: plan.skipped,
        unresolved: plan
            .unresolved
            .into_iter()
            .map(|(view, name)| UnresolvedViewName { view, name })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A CBOR text value, decoded from its bytes.
    fn text(s: &str) -> sunrise_cbor::CborValue {
        let mut bytes = vec![0x60 + u8::try_from(s.len()).unwrap()];
        bytes.extend_from_slice(s.as_bytes());
        sunrise_cbor::decode_lenient(&bytes).unwrap()
    }

    /// A part a newer build added to the filter, and to its window, survives
    /// the trip out to a client and back.
    #[test]
    fn a_filter_keeps_what_a_newer_build_added_across_the_seam() {
        let mut f = ViewFilter {
            window: Some(DateWindow {
                field: WindowField::Due,
                from_days: None,
                to_days: Some(0),
                unknown: Unknowns::new(),
            }),
            text: "milk".into(),
            ..ViewFilter::default()
        };
        f.unknown.insert("place".into(), text("plc_x"));
        if let Some(w) = f.window.as_mut() {
            w.unknown.insert("zone".into(), text("home"));
        }
        let item = ViewFilterItem::from(&f);
        assert!(item.extra.is_some());
        assert_eq!(ViewFilter::from(item.clone()), f);
        // An edit of a typed part keeps the rest.
        let edited = ViewFilter::from(ViewFilterItem {
            text: "oat milk".into(),
            ..item
        });
        assert_eq!(edited.text, "oat milk");
        assert_eq!(edited.unknown, f.unknown);
        assert_eq!(
            edited.window.map(|w| w.unknown),
            f.window.map(|w| w.unknown)
        );
    }

    #[test]
    fn a_plain_filter_needs_no_extra() {
        let f = ViewFilter {
            contexts: vec![EntityRef::new(sunrise_id::EntityKind::Context, [1; 16])],
            ..ViewFilter::default()
        };
        let item = ViewFilterItem::from(&f);
        assert_eq!(item.extra, None);
        assert_eq!(ViewFilter::from(item), f);
    }
}

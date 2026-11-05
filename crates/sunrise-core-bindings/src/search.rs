//! Search across the seam (issue #345, ADR-0052):
//! [`sunrise_core::SearchResults`], mirrored. The parsing, the grouping, the
//! ranking and the highlight ranges are the core's; a client draws the
//! groups in the order they arrive and the ranges it is given, and never
//! re-tokenizes.

use sunrise_core::{SearchGroup, SearchHit, SearchNotice, SearchResults};
use sunrise_domain::search::{Highlighted, SearchKind};
use sunrise_id::EntityRef;

/// A kind of search hit. The declaration order is the order groups come in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SearchKindItem {
    /// A task.
    Task,
    /// A note.
    Note,
    /// An external calendar event.
    Event,
    /// A time block.
    Block,
    /// A stream.
    Stream,
    /// A context.
    Context,
    /// A place.
    Place,
    /// An attachment, found by its filename.
    Attachment,
    /// A saved view.
    View,
    /// A person.
    Person,
}

impl From<SearchKind> for SearchKindItem {
    fn from(k: SearchKind) -> Self {
        match k {
            SearchKind::Task => Self::Task,
            SearchKind::Note => Self::Note,
            SearchKind::Event => Self::Event,
            SearchKind::Block => Self::Block,
            SearchKind::Stream => Self::Stream,
            SearchKind::Context => Self::Context,
            SearchKind::Place => Self::Place,
            SearchKind::Attachment => Self::Attachment,
            SearchKind::View => Self::View,
            SearchKind::Person => Self::Person,
        }
    }
}

/// One matched range: Unicode-scalar offsets into the text it belongs to,
/// half-open. Swift's `String.unicodeScalars` indexes by the same unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct HighlightRange {
    /// The first matched scalar.
    pub start: u32,
    /// The scalar after the last.
    pub end: u32,
}

/// Text with the ranges of it that matched, sorted and not overlapping.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct HighlightedText {
    /// The text, as shown.
    pub text: String,
    /// What matched.
    pub ranges: Vec<HighlightRange>,
}

impl From<&Highlighted> for HighlightedText {
    fn from(h: &Highlighted) -> Self {
        Self {
            text: h.text.clone(),
            ranges: h
                .ranges
                .iter()
                .map(|&(start, end)| HighlightRange { start, end })
                .collect(),
        }
    }
}

/// One hit.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SearchHitItem {
    /// What matched.
    pub entity: EntityRef,
    /// What activating the hit opens: the entity, or for an attachment the
    /// entity it is attached to.
    pub opens: EntityRef,
    /// The title or name, highlighted.
    pub title: HighlightedText,
    /// At most 160 scalars of the body around its first match; absent when
    /// the body did not match.
    pub snippet: Option<HighlightedText>,
}

impl From<&SearchHit> for SearchHitItem {
    fn from(h: &SearchHit) -> Self {
        Self {
            entity: h.entity,
            opens: h.opens,
            title: HighlightedText::from(&h.title),
            snippet: h.snippet.as_ref().map(HighlightedText::from),
        }
    }
}

/// The hits of one kind.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SearchGroupItem {
    /// The kind.
    pub kind: SearchKindItem,
    /// Up to the group's limit, best first.
    pub hits: Vec<SearchHitItem>,
    /// How many there were before the limit: "Show all N".
    pub total: u32,
}

impl From<&SearchGroup> for SearchGroupItem {
    fn from(g: &SearchGroup) -> Self {
        Self {
            kind: g.kind.into(),
            hits: g.hits.iter().map(SearchHitItem::from).collect(),
            total: g.total,
        }
    }
}

/// Why part of a query was read as text, left out, or matched nothing:
/// shown as a subtle underline under `start..end` of the field.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SearchNoticeItem {
    /// A stable code a client localizes: `unknown_key`, `bad_value`,
    /// `unclosed_quote`, `unclosed_group`, `stray_paren`, `empty_phrase`,
    /// `literal_or`, `too_long`, `too_many_clauses`, `too_deep`,
    /// `unknown_name`, `ambiguous_name`, `unsupported`. A newer core may add
    /// codes; show one this client does not know as a plain underline.
    pub code: String,
    /// Where it starts in the query, in Unicode scalars.
    pub start: u32,
    /// Where it ends, half-open.
    pub end: u32,
    /// The name or operator it is about, when it is about one.
    pub detail: Option<String>,
}

impl From<&SearchNotice> for SearchNoticeItem {
    fn from(n: &SearchNotice) -> Self {
        Self {
            code: n.kind.code().to_owned(),
            start: n.start,
            end: n.end,
            detail: n.detail.clone(),
        }
    }
}

/// What a search found.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SearchReport {
    /// One group per kind with a hit, in the fixed order.
    pub groups: Vec<SearchGroupItem>,
    /// What the query read as text or matched nothing for.
    pub notices: Vec<SearchNoticeItem>,
    /// The query's canonical text: what ⌘S saves.
    pub canonical: String,
}

impl From<&SearchResults> for SearchReport {
    fn from(r: &SearchResults) -> Self {
        Self {
            groups: r.groups.iter().map(SearchGroupItem::from).collect(),
            notices: r.notices.iter().map(SearchNoticeItem::from).collect(),
            canonical: r.canonical.clone(),
        }
    }
}

/// How far a re-index of the word index has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct SearchReindexItem {
    /// Docs copied into the new index.
    pub copied: u64,
    /// Docs in the index.
    pub total: u64,
    /// Whether the index now follows the content language.
    pub finished: bool,
}

impl From<sunrise_core::SearchReindex> for SearchReindexItem {
    fn from(r: sunrise_core::SearchReindex) -> Self {
        Self {
            copied: r.copied,
            total: r.total,
            finished: r.finished,
        }
    }
}

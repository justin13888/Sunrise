//! Search across the seam (issue #345, ADR-0052):
//! [`sunrise_core::SearchResults`], mirrored. The parsing, the grouping, the
//! ranking and the highlight ranges are the core's; a client draws the
//! groups in the order they arrive and the ranges it is given, and never
//! re-tokenizes.

use sunrise_core::{SearchGroup, SearchHit, SearchNotice, SearchResults};
use sunrise_domain::search::{
    complete, scope_token, Completion, CompletionKind, CompletionNames, Highlighted, Scope,
    SearchKind,
};
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

impl From<SearchKindItem> for SearchKind {
    fn from(k: SearchKindItem) -> Self {
        match k {
            SearchKindItem::Task => Self::Task,
            SearchKindItem::Note => Self::Note,
            SearchKindItem::Event => Self::Event,
            SearchKindItem::Block => Self::Block,
            SearchKindItem::Stream => Self::Stream,
            SearchKindItem::Context => Self::Context,
            SearchKindItem::Place => Self::Place,
            SearchKindItem::Attachment => Self::Attachment,
            SearchKindItem::View => Self::View,
            SearchKindItem::Person => Self::Person,
        }
    }
}

/// The `kind:` keyword that restricts a query to `kind`: what a group's
/// "Show all" adds to the field.
#[uniffi::export]
#[must_use]
pub fn search_kind_keyword(kind: SearchKindItem) -> String {
    SearchKind::from(kind).as_str().to_owned()
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

/// What an autocomplete suggestion is. A client describes it by this and
/// by [`SearchCompletionItem::key`] and [`SearchCompletionItem::value`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SearchCompletionKindItem {
    /// An operator key (`is:`, `due:`, …).
    Key,
    /// A value of an operator (`late` after `is:`, `today` after `due:`).
    Value,
    /// A stream's name, after `#` or `stream:`.
    Stream,
    /// A context's name, after `@` or `context:`.
    Context,
}

/// One way to finish the word at the end of the search field. See
/// [`sunrise_domain::search::complete`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SearchCompletionItem {
    /// The whole field after accepting it.
    pub text: String,
    /// The operator, value or name as it will read in the field.
    pub label: String,
    /// What it is.
    pub kind: SearchCompletionKindItem,
    /// The operator it belongs to: the key itself for a key, the operator
    /// for a value (`date` for every date operator), and `stream` or
    /// `context` for a name.
    pub key: String,
    /// The value's keyword, for a value.
    pub value: Option<String>,
}

impl From<Completion> for SearchCompletionItem {
    fn from(c: Completion) -> Self {
        let (kind, key, value) = match c.kind {
            CompletionKind::Key(k) => (SearchCompletionKindItem::Key, k, None),
            CompletionKind::Value { key, value } => {
                (SearchCompletionKindItem::Value, key, Some(value.to_owned()))
            }
            CompletionKind::Stream => (SearchCompletionKindItem::Stream, "stream", None),
            CompletionKind::Context => (SearchCompletionKindItem::Context, "context", None),
        };
        Self {
            text: c.text,
            label: c.label,
            kind,
            key: key.to_owned(),
            value,
        }
    }
}

/// The ways to finish the word at the end of `text`, best first: operator
/// keys, their values, and the stream and context names given. Pure and
/// cheap, so a field may ask on every keystroke; the names are the caller's,
/// read once with the list it already shows.
#[uniffi::export]
#[must_use]
pub fn search_completions(
    text: String,
    streams: Vec<String>,
    contexts: Vec<String>,
) -> Vec<SearchCompletionItem> {
    complete(&text, &CompletionNames { streams, contexts })
        .into_iter()
        .map(SearchCompletionItem::from)
        .collect()
}

/// What a list the search field is opened from is scoped to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SearchScopeItem {
    /// A stream's list.
    Stream,
    /// A context's list.
    Context,
}

/// The editable token the field is prefilled with when search is opened
/// from a stream's or a context's list: `#work `, `@errands `, the name
/// quoted where the grammar needs it.
#[uniffi::export]
#[must_use]
pub fn search_scope_token(scope: SearchScopeItem, name: String) -> String {
    let scope = match scope {
        SearchScopeItem::Stream => Scope::Stream,
        SearchScopeItem::Context => Scope::Context,
    };
    scope_token(scope, &name)
}

//! The parsed form of a query: what [`super::parse()`] returns and
//! [`std::fmt::Display`] prints back.

use super::date::DateSpan;
use serde::{Deserialize, Serialize};

/// A parsed query: its clauses, joined by AND.
///
/// Built only by [`super::parse()`], and printed by `Display` in the canonical
/// form, which parses back to the same value. An empty query has no clauses
/// and matches nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SearchQuery {
    /// The clauses, every one of which a hit matches.
    pub clauses: Vec<Expr>,
}

impl SearchQuery {
    /// Whether the query has no clauses, and so matches nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.clauses.is_empty()
    }
}

/// One node of a query.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Expr {
    /// A bare word, matched as a word or a substring. Never empty, and never
    /// holds whitespace unless it is the text of an operator the parser could
    /// not read, which it keeps whole.
    Term(String),
    /// A quoted phrase, matched as its words in order or as a substring.
    /// Never empty and never holds `"`.
    Phrase(String),
    /// A structured operator.
    Filter(Filter),
    /// `-atom`: the atom does not match.
    Not(Box<Expr>),
    /// `a OR b OR …`: at least one matches. Always two or more.
    Or(Vec<Expr>),
    /// `( … )`: every clause inside matches.
    Group(Vec<Expr>),
}

/// A structured operator, after its value was read.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Filter {
    /// `stream:` or `#`: tasks and blocks in one of these streams or a stream
    /// nested under one. Names as typed, resolved at query time.
    Stream(Vec<String>),
    /// `context:` or `@`: tasks carrying one of these contexts.
    Context(Vec<String>),
    /// `place:`: tasks requiring one of these places.
    Place(Vec<String>),
    /// `state:`: tasks in one of these states.
    State(Vec<StateValue>),
    /// `kind:`: hits of one of these kinds.
    Kind(Vec<SearchKind>),
    /// `is:`: hits with one of these properties.
    Is(Vec<IsValue>),
    /// `has:`: hits with one of these parts.
    Has(Vec<HasValue>),
    /// `priority` or `!`: a priority from 1 to 5. Several values only with
    /// [`Cmp::Eq`], OR'd.
    Priority(Cmp, Vec<u8>),
    /// A date operator: the field, the comparison, and the day or period.
    Date(DateField, Cmp, DateSpan),
}

/// How a filter compares its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Cmp {
    /// `:`: equal to, or within the day or period.
    Eq,
    /// `<`: before.
    Lt,
    /// `<=`: before, or within.
    Le,
    /// `>`: after.
    Gt,
    /// `>=`: within, or after.
    Ge,
}

impl Cmp {
    /// The comparator as typed.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Eq => ":",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }
}

/// Which date a date operator reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DateField {
    /// `planned`: when the task is planned for (`planned_at`).
    Planned,
    /// `target`: the soft deadline (`target_at`).
    Target,
    /// `hard`: the hard deadline (`hard_due_at`).
    Hard,
    /// `due`: the earlier of the two deadlines.
    Due,
    /// `created`: when the entity was created.
    Created,
    /// `done`: when the task was completed.
    Done,
}

impl DateField {
    /// Every field, in the order the grammar lists them.
    pub const ALL: [Self; 6] = [
        Self::Planned,
        Self::Target,
        Self::Hard,
        Self::Due,
        Self::Created,
        Self::Done,
    ];

    /// The operator key.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Target => "target",
            Self::Hard => "hard",
            Self::Due => "due",
            Self::Created => "created",
            Self::Done => "done",
        }
    }
}

/// A `state:` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum StateValue {
    /// `todo`.
    Todo,
    /// `in_progress`.
    InProgress,
    /// `done`.
    Done,
    /// `cancelled`.
    Cancelled,
    /// `open`: `todo` or `in_progress`.
    Open,
}

/// An `is:` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum IsValue {
    /// `late`: past a deadline, soft or hard, and still open.
    Late,
    /// `late-hard`: past the hard deadline and still open.
    LateHard,
    /// `stale`: an open task with no deadline, untouched for
    /// `stale_after_days` planner days (ADR-0047 §2).
    Stale,
    /// `blocked`: an open task with an open blocker.
    Blocked,
    /// `recurring`: generated by a routine.
    Recurring,
    /// `unplanned`: an open task with no planned time.
    Unplanned,
    /// `inbox`: a task in the Inbox view: open, with no stream, no plan, no
    /// deadline and no `triaged_at` (ADR-0046 §3).
    Inbox,
    /// `fixed`: a block that does not move. Not available yet.
    Fixed,
    /// `flexible`: a block that may move. Not available yet.
    Flexible,
}

/// A `has:` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum HasValue {
    /// `attachment`: a live attachment.
    Attachment,
    /// `note`: a non-empty note body.
    Note,
    /// `estimate`: an estimated duration.
    Estimate,
    /// `deadline`: a deadline.
    Deadline,
    /// `place`: a required place. Not available yet.
    Place,
    /// `dependency`: at least one blocker.
    Dependency,
}

/// A kind of hit. The declaration order is the fixed order results are
/// grouped in (`docs/08-features/search.md` §Results).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SearchKind {
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
    /// An attachment, by filename.
    Attachment,
    /// A saved view.
    View,
    /// A person.
    Person,
}

impl SearchKind {
    /// Every kind, in group order.
    pub const ALL: [Self; 10] = [
        Self::Task,
        Self::Note,
        Self::Event,
        Self::Block,
        Self::Stream,
        Self::Context,
        Self::Place,
        Self::Attachment,
        Self::View,
        Self::Person,
    ];

    /// The `kind:` keyword, which is also the index's `kind` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Note => "note",
            Self::Event => "event",
            Self::Block => "block",
            Self::Stream => "stream",
            Self::Context => "context",
            Self::Place => "place",
            Self::Attachment => "attachment",
            Self::View => "view",
            Self::Person => "person",
        }
    }

    /// The kind a `kind:` keyword or an index row's `kind` names.
    #[must_use]
    pub fn from_keyword(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// A keyword vocabulary: each value and its spelling.
pub(super) trait Keyword: Copy + Sized + 'static {
    const ALL: &'static [Self];
    fn keyword(self) -> &'static str;
    fn from_keyword(s: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|v| v.keyword().eq_ignore_ascii_case(s))
    }
}

impl Keyword for StateValue {
    const ALL: &'static [Self] = &[
        Self::Todo,
        Self::InProgress,
        Self::Done,
        Self::Cancelled,
        Self::Open,
    ];
    fn keyword(self) -> &'static str {
        match self {
            Self::Todo => "todo",
            Self::InProgress => "in_progress",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
            Self::Open => "open",
        }
    }
}

impl Keyword for IsValue {
    const ALL: &'static [Self] = &[
        Self::Late,
        Self::LateHard,
        Self::Stale,
        Self::Blocked,
        Self::Recurring,
        Self::Unplanned,
        Self::Inbox,
        Self::Fixed,
        Self::Flexible,
    ];
    fn keyword(self) -> &'static str {
        match self {
            Self::Late => "late",
            Self::LateHard => "late-hard",
            Self::Stale => "stale",
            Self::Blocked => "blocked",
            Self::Recurring => "recurring",
            Self::Unplanned => "unplanned",
            Self::Inbox => "inbox",
            Self::Fixed => "fixed",
            Self::Flexible => "flexible",
        }
    }
}

impl Keyword for HasValue {
    const ALL: &'static [Self] = &[
        Self::Attachment,
        Self::Note,
        Self::Estimate,
        Self::Deadline,
        Self::Place,
        Self::Dependency,
    ];
    fn keyword(self) -> &'static str {
        match self {
            Self::Attachment => "attachment",
            Self::Note => "note",
            Self::Estimate => "estimate",
            Self::Deadline => "deadline",
            Self::Place => "place",
            Self::Dependency => "dependency",
        }
    }
}

impl Keyword for SearchKind {
    const ALL: &'static [Self] = &Self::ALL;
    fn keyword(self) -> &'static str {
        self.as_str()
    }
}

impl StateValue {
    /// The keyword, as `state:` takes it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.keyword()
    }
}

impl IsValue {
    /// The keyword, as `is:` takes it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.keyword()
    }
}

impl HasValue {
    /// The keyword, as `has:` takes it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.keyword()
    }
}

/// Something the parser read as text, or left out, and why. Offsets are
/// Unicode scalars into the input, half-open.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ParseNotice {
    /// What happened.
    pub kind: NoticeKind,
    /// Where it starts.
    pub start: u32,
    /// Where it ends.
    pub end: u32,
}

/// The reasons a [`ParseNotice`] gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NoticeKind {
    /// `word:` where `word` is no operator: read as text.
    UnknownKey,
    /// A known operator whose value it cannot read: the whole token is text.
    BadValue,
    /// A `"` with no closing one: the phrase runs to the end.
    UnclosedQuote,
    /// A `(` with no `)`: closed at the end.
    UnclosedGroup,
    /// A `)` with no `(`: ignored.
    StrayParen,
    /// `""`: ignored.
    EmptyPhrase,
    /// `OR` with nothing on one side: read as the word.
    LiteralOr,
    /// Past the text budget ([`super::MAX_TEXT`]): the rest is ignored.
    TooLong,
    /// Past [`super::MAX_CLAUSES`] clauses: the rest is one phrase.
    TooManyClauses,
    /// Past [`super::MAX_DEPTH`] nested groups: the rest is one phrase.
    TooDeep,
    /// A stream, context or place name that names nothing. Raised when the
    /// query runs, not by the parser.
    UnknownName,
    /// A name that more than one stream or context starts with. Raised when
    /// the query runs.
    AmbiguousName,
    /// An operator this build parses but cannot evaluate yet, which matches
    /// nothing. Raised when the query runs.
    Unsupported,
}

impl NoticeKind {
    /// A stable code for the notice, which clients localize.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::UnknownKey => "unknown_key",
            Self::BadValue => "bad_value",
            Self::UnclosedQuote => "unclosed_quote",
            Self::UnclosedGroup => "unclosed_group",
            Self::StrayParen => "stray_paren",
            Self::EmptyPhrase => "empty_phrase",
            Self::LiteralOr => "literal_or",
            Self::TooLong => "too_long",
            Self::TooManyClauses => "too_many_clauses",
            Self::TooDeep => "too_deep",
            Self::UnknownName => "unknown_name",
            Self::AmbiguousName => "ambiguous_name",
            Self::Unsupported => "unsupported",
        }
    }
}

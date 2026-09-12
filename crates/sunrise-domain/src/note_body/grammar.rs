//! The shape of a note body, and the limits it is read under.
//!
//! The grammar `docs/02-domain/notes.md` §Body grammar names — the block,
//! inline and mark types, the constructors and wire-string parsers that go
//! with them — plus [`Fidelity`] and [`NoteDoc`], which say what a reader is
//! allowed to *do* with a decoded body. These belong together because they are
//! the vocabulary the three directions are all written against: no CBOR, no
//! Markdown, no I/O, and no dependency on any of the modules beside it.

/// Soft length limit for one body, per `docs/02-domain/notes.md` §Length.
/// Past this, performance degrades and the UI should nudge toward splitting.
pub const NOTE_BODY_SOFT_LIMIT_BYTES: usize = 64 * 1024;

/// Length limit for one body, per `docs/02-domain/notes.md` §Length.
///
/// Advisory, and deliberately not enforced by this module: neither
/// [`encode`](super::encode()) nor [`decode`](super::decode()) compares against
/// it, and a larger body round-trips intact.
/// It is published for the editing surface to apply -- where a user can be
/// warned before they lose work, rather than after the codec has refused it --
/// and that is the only place it binds today.
pub const NOTE_BODY_MAX_BYTES: usize = 1024 * 1024;

/// How deep list nesting may go before [`decode`](super::decode()) stops
/// descending.
///
/// `ListItem.children` is unbounded in the grammar, and a decoder that
/// followed it without a floor would let a hostile — or merely corrupt — body
/// overflow the stack. Children below this depth are dropped, which makes the
/// body [`Fidelity::Lossy`] and therefore read-only, rather than fatal.
pub const MAX_NEST_DEPTH: usize = 8;

/// The most paragraphs a plain-text body is rendered as.
///
/// Only reachable through the non-CBOR fallback, where the input is already
/// outside the grammar. A 1 MB body of single characters would otherwise
/// become half a million blocks.
pub(super) const MAX_PLAIN_TEXT_PARAGRAPHS: usize = 4096;

/// An empty document: CBOR `[]`.
pub(super) const EMPTY_DOC: [u8; 1] = [0x80];

// ---------------------------------------------------------------------------
// Grammar
// ---------------------------------------------------------------------------

/// One inline mark. `Mark = "bold" / "italic" / "underline" / "strike" /
/// "code"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mark {
    /// `**bold**`.
    Bold,
    /// `*italic*`.
    Italic,
    /// Underline. Markdown has no syntax for it; export emits `<u>`.
    Underline,
    /// `~~struck through~~`.
    Strike,
    /// `` `code` ``.
    Code,
}

impl Mark {
    /// The stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bold => "bold",
            Self::Italic => "italic",
            Self::Underline => "underline",
            Self::Strike => "strike",
            Self::Code => "code",
        }
    }

    /// Parse a wire string. Unlike [`crate::Energy`], an unknown mark has no
    /// sensible middle rung, so this returns `None` and the caller drops the
    /// mark — which makes the body [`Fidelity::Lossy`] and read-only.
    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        Some(match s {
            "bold" => Self::Bold,
            "italic" => Self::Italic,
            "underline" => Self::Underline,
            "strike" => Self::Strike,
            "code" => Self::Code,
            _ => return None,
        })
    }

    /// Every mark, in wire order.
    pub const ALL: [Self; 5] = [
        Self::Bold,
        Self::Italic,
        Self::Underline,
        Self::Strike,
        Self::Code,
    ];
}

/// A heading level. The grammar says `level: 1..3`, so the type says it too:
/// an editor cannot construct a heading the grammar cannot express.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HeadingLevel {
    /// `#`.
    One,
    /// `##`.
    Two,
    /// `###`.
    Three,
}

impl HeadingLevel {
    /// The wire integer, 1..=3.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::One => 1,
            Self::Two => 2,
            Self::Three => 3,
        }
    }

    /// Nearest representable level. Out-of-range input is clamped rather than
    /// refused: a level-6 heading from some other editor should still render
    /// as a heading. The clamp is what makes the body [`Fidelity::Lossy`].
    #[must_use]
    pub const fn clamp_from(level: i64) -> Self {
        match level {
            ..=1 => Self::One,
            2 => Self::Two,
            _ => Self::Three,
        }
    }
}

/// One piece of inline content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    /// `{text: text, marks?: [* Mark]}`.
    Text {
        /// The characters.
        text: String,
        /// Marks applied to the whole run. Sorted and deduplicated by
        /// [`encode`](super::encode()) so that two editors that chose
        /// different orders produce the same bytes.
        marks: Vec<Mark>,
    },
    /// `{kind: "link", href: text, label: text}`.
    Link {
        /// Where it points.
        href: String,
        /// What it reads as.
        label: String,
    },
    /// `{kind: "ref", ref: tstr}` — an in-app entity link, rendered as the
    /// target's title by whoever knows the title. This crate does not.
    Ref {
        /// The target entity id, as text.
        target: String,
    },
    /// `{kind: "mention", person: tstr}`.
    Mention {
        /// The person id, as text.
        person: String,
    },
    /// `{kind: "redacted", reason: …, placeholder_text: …}` — what a [`Self::Ref`]
    /// becomes when scrubbed at egress, or when its target is soft-deleted.
    ///
    /// `reason` is carried as text, not as an enum, because the grammar
    /// spells it as a string union (`"private_ref" / "external_account" /
    /// "deleted_entity"`) and a reason this build has not heard of should
    /// still render its placeholder rather than vanish.
    Redacted {
        /// Why the original was withheld.
        reason: String,
        /// The visible token, e.g. `(redacted)`.
        placeholder_text: String,
    },
}

impl Inline {
    /// An unmarked run of text.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            marks: Vec::new(),
        }
    }

    /// The characters this run contributes to a plain-text rendering.
    #[must_use]
    pub fn plain_text(&self) -> &str {
        match self {
            Self::Text { text, .. } => text,
            Self::Link { label, .. } => label,
            Self::Ref { target } => target,
            Self::Mention { person } => person,
            Self::Redacted {
                placeholder_text, ..
            } => placeholder_text,
        }
    }
}

/// One item of a `ul` / `ol`, which may nest further blocks beneath it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListItem {
    /// The item's own inline content.
    pub inline: Vec<Inline>,
    /// Blocks nested under it. Empty is the common case and is omitted from
    /// the encoding entirely.
    pub children: Vec<NoteBlock>,
}

/// One row of a checklist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChecklistItem {
    /// Whether the box is ticked.
    pub checked: bool,
    /// The row's inline content.
    pub inline: Vec<Inline>,
}

/// A block of note content.
///
/// **Deliberately not called `Block`** — that name is the time-block entity in
/// `docs/02-domain/time-blocks.md`, and the two are unrelated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteBlock {
    /// `{kind: "p", inline: [* Inline]}`.
    Paragraph {
        /// Inline content.
        inline: Vec<Inline>,
    },
    /// `{kind: "h", level: 1..3, inline: [* Inline]}`.
    Heading {
        /// 1..=3.
        level: HeadingLevel,
        /// Inline content.
        inline: Vec<Inline>,
    },
    /// `{kind: "ul" / "ol", items: [+ ListItem]}`.
    List {
        /// `ol` when true, `ul` when false.
        ordered: bool,
        /// The items.
        items: Vec<ListItem>,
    },
    /// `{kind: "task", items: [+ ChecklistItem]}`.
    Checklist {
        /// The rows.
        items: Vec<ChecklistItem>,
    },
    /// `{kind: "code", language?: text, content: text}`.
    Code {
        /// Language hint, for highlighting.
        language: Option<String>,
        /// The literal source. No inline content: code is not marked up.
        content: String,
    },
    /// `{kind: "quote", inline: [* Inline]}`.
    Quote {
        /// Inline content.
        inline: Vec<Inline>,
    },
    /// `{kind: "hr"}`.
    Divider,
}

// ---------------------------------------------------------------------------
// Fidelity
// ---------------------------------------------------------------------------

/// Whether this build understood a body completely enough to write it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fidelity {
    /// Re-encoding the decoded blocks reproduces the input bytes exactly.
    /// Saving over this body loses nothing.
    Exact,
    /// The input held something this build could not put back: an unknown
    /// block kind or mark, a key it does not model, nesting past
    /// [`MAX_NEST_DEPTH`], a non-canonical encoding, or bytes that were never
    /// the grammar at all.
    ///
    /// The blocks are still the best rendering available — that is the point
    /// — but an editor **must not save over the original**. It would drop
    /// whatever made this lossy, and a body is one LWW unit, so the drop
    /// would then propagate to every other device.
    Lossy,
}

/// What [`decode`](super::decode()) made of a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteDoc {
    /// The best rendering this build can manage. Never an error, possibly
    /// empty.
    pub blocks: Vec<NoteBlock>,
    /// Whether [`blocks`](Self::blocks) can be safely written back over the
    /// input.
    pub fidelity: Fidelity,
}

impl NoteDoc {
    /// Whether the body round-trips through this build without loss, and is
    /// therefore safe to edit and save.
    #[must_use]
    pub fn is_exact(&self) -> bool {
        matches!(self.fidelity, Fidelity::Exact)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spec_length_limits_are_the_ones_the_document_states() {
        assert_eq!(NOTE_BODY_SOFT_LIMIT_BYTES, 65_536);
        assert_eq!(NOTE_BODY_MAX_BYTES, 1_048_576);
    }
}

//! The `NoteBody` block grammar, as structured values rather than bytes.
//!
//! `docs/02-domain/notes.md` puts a CBOR block grammar *inside* a NoteBody's
//! opaque bytes. [`sunrise_domain::note_body`] is the codec; this module is
//! the seam over it, and it exists so that **no client parses CBOR**.
//!
//! Handing Swift the raw bytes would mean a second decoder, written in a
//! language with no proptest against the first, drifting from it the first
//! time either side met a body the other wrote. The spec's own framing makes
//! that the whole risk: the grammar is "the contract editors and renderers
//! agree on", so there had better be one implementation of it.
//!
//! # `NoteFidelity` is not decoration
//!
//! [`decode_note_body`] reports whether this build can put a body back
//! exactly as it found it. When it cannot — a block kind from a newer build,
//! a mark it does not know, a plain-text body from the CLI — the blocks are
//! still the best rendering available, but **an editor must not save over the
//! original**. A NoteBody is one last-writer-wins unit (ADR-0014), so a
//! dropped block does not stay local: it propagates to every device that
//! syncs afterwards. See the module docs on the codec.
//!
//! # Naming
//!
//! Everything carries a `Note` prefix. `NoteBlock` is the spec's own name and
//! is deliberately not `Block` — that is the time-block entity, and the two
//! are unrelated. The rest follow for consistency, and because `Divider`,
//! `Text` and `List` are all names SwiftUI already owns.

use sunrise_domain::note_body;
use sunrise_domain::NoteBody;

// ---------------------------------------------------------------------------
// Grammar
// ---------------------------------------------------------------------------

/// See [`sunrise_domain::note_body::Mark`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NoteMark {
    /// Bold.
    Bold,
    /// Italic.
    Italic,
    /// Underline.
    Underline,
    /// Struck through.
    Strike,
    /// Inline code.
    Code,
}

impl From<note_body::Mark> for NoteMark {
    fn from(m: note_body::Mark) -> Self {
        match m {
            note_body::Mark::Bold => Self::Bold,
            note_body::Mark::Italic => Self::Italic,
            note_body::Mark::Underline => Self::Underline,
            note_body::Mark::Strike => Self::Strike,
            note_body::Mark::Code => Self::Code,
        }
    }
}

impl From<NoteMark> for note_body::Mark {
    fn from(m: NoteMark) -> Self {
        match m {
            NoteMark::Bold => Self::Bold,
            NoteMark::Italic => Self::Italic,
            NoteMark::Underline => Self::Underline,
            NoteMark::Strike => Self::Strike,
            NoteMark::Code => Self::Code,
        }
    }
}

/// See [`sunrise_domain::note_body::HeadingLevel`].
///
/// An enum rather than an integer, on both sides of the seam: the grammar
/// says `level: 1..3`, so a client cannot ask for a level-6 heading and find
/// out later that it was silently clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NoteHeadingLevel {
    /// Largest.
    One,
    /// Middle.
    Two,
    /// Smallest.
    Three,
}

impl From<note_body::HeadingLevel> for NoteHeadingLevel {
    fn from(l: note_body::HeadingLevel) -> Self {
        match l {
            note_body::HeadingLevel::One => Self::One,
            note_body::HeadingLevel::Two => Self::Two,
            note_body::HeadingLevel::Three => Self::Three,
        }
    }
}

impl From<NoteHeadingLevel> for note_body::HeadingLevel {
    fn from(l: NoteHeadingLevel) -> Self {
        match l {
            NoteHeadingLevel::One => Self::One,
            NoteHeadingLevel::Two => Self::Two,
            NoteHeadingLevel::Three => Self::Three,
        }
    }
}

/// See [`sunrise_domain::note_body::Inline`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum NoteInline {
    /// A run of text, with the marks that apply to all of it.
    Text {
        /// The characters.
        text: String,
        /// Marks over the whole run.
        marks: Vec<NoteMark>,
    },
    /// An external link.
    Link {
        /// Where it points.
        href: String,
        /// What it reads as.
        label: String,
    },
    /// An in-app entity link. Renders as the target's title, which this seam
    /// does not know — the client resolves it.
    Ref {
        /// The target entity id.
        target: String,
    },
    /// A person mention.
    Mention {
        /// The person id.
        person: String,
    },
    /// A reference withheld at egress, or whose target was soft-deleted.
    ///
    /// Renderable but **not authorable**: nothing in an editor produces one.
    /// `reason` is text, not an enum, because the grammar spells it as a
    /// string union and a reason this build has not heard of should still
    /// render its placeholder.
    Redacted {
        /// `private_ref` / `external_account` / `deleted_entity`.
        reason: String,
        /// The visible token, e.g. `(redacted)`.
        placeholder_text: String,
    },
}

impl From<note_body::Inline> for NoteInline {
    fn from(i: note_body::Inline) -> Self {
        match i {
            note_body::Inline::Text { text, marks } => Self::Text {
                text,
                marks: marks.into_iter().map(NoteMark::from).collect(),
            },
            note_body::Inline::Link { href, label } => Self::Link { href, label },
            note_body::Inline::Ref { target } => Self::Ref { target },
            note_body::Inline::Mention { person } => Self::Mention { person },
            note_body::Inline::Redacted {
                reason,
                placeholder_text,
            } => Self::Redacted {
                reason,
                placeholder_text,
            },
        }
    }
}

impl From<NoteInline> for note_body::Inline {
    fn from(i: NoteInline) -> Self {
        match i {
            NoteInline::Text { text, marks } => Self::Text {
                text,
                marks: marks.into_iter().map(note_body::Mark::from).collect(),
            },
            NoteInline::Link { href, label } => Self::Link { href, label },
            NoteInline::Ref { target } => Self::Ref { target },
            NoteInline::Mention { person } => Self::Mention { person },
            NoteInline::Redacted {
                reason,
                placeholder_text,
            } => Self::Redacted {
                reason,
                placeholder_text,
            },
        }
    }
}

/// See [`sunrise_domain::note_body::ListItem`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NoteListItem {
    /// The item's own inline content.
    pub inline: Vec<NoteInline>,
    /// Blocks nested under it. Empty is the common case.
    pub children: Vec<NoteBlock>,
}

impl From<note_body::ListItem> for NoteListItem {
    fn from(item: note_body::ListItem) -> Self {
        // Destructured exhaustively, no `..`: a field added upstream breaks
        // this build rather than being dropped on the way to the client.
        let note_body::ListItem { inline, children } = item;
        Self {
            inline: inline.into_iter().map(NoteInline::from).collect(),
            children: children.into_iter().map(NoteBlock::from).collect(),
        }
    }
}

impl From<NoteListItem> for note_body::ListItem {
    fn from(item: NoteListItem) -> Self {
        let NoteListItem { inline, children } = item;
        Self {
            inline: inline.into_iter().map(note_body::Inline::from).collect(),
            children: children
                .into_iter()
                .map(note_body::NoteBlock::from)
                .collect(),
        }
    }
}

/// See [`sunrise_domain::note_body::ChecklistItem`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NoteChecklistItem {
    /// Whether the box is ticked.
    pub checked: bool,
    /// The row's inline content.
    pub inline: Vec<NoteInline>,
}

impl From<note_body::ChecklistItem> for NoteChecklistItem {
    fn from(item: note_body::ChecklistItem) -> Self {
        let note_body::ChecklistItem { checked, inline } = item;
        Self {
            checked,
            inline: inline.into_iter().map(NoteInline::from).collect(),
        }
    }
}

impl From<NoteChecklistItem> for note_body::ChecklistItem {
    fn from(item: NoteChecklistItem) -> Self {
        let NoteChecklistItem { checked, inline } = item;
        Self {
            checked,
            inline: inline.into_iter().map(note_body::Inline::from).collect(),
        }
    }
}

/// See [`sunrise_domain::note_body::NoteBlock`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum NoteBlock {
    /// A paragraph.
    Paragraph {
        /// Inline content.
        inline: Vec<NoteInline>,
    },
    /// A heading, levels one to three.
    Heading {
        /// How large.
        level: NoteHeadingLevel,
        /// Inline content.
        inline: Vec<NoteInline>,
    },
    /// A bulleted or numbered list, whose items may nest further blocks.
    List {
        /// Numbered when true, bulleted when false.
        ordered: bool,
        /// The items.
        items: Vec<NoteListItem>,
    },
    /// A checklist.
    Checklist {
        /// The rows.
        items: Vec<NoteChecklistItem>,
    },
    /// A code block. Its content is literal — no inline marks.
    Code {
        /// Language hint, for highlighting.
        language: Option<String>,
        /// The literal source.
        content: String,
    },
    /// A block quote.
    Quote {
        /// Inline content.
        inline: Vec<NoteInline>,
    },
    /// A horizontal rule.
    Divider,
}

impl From<note_body::NoteBlock> for NoteBlock {
    fn from(b: note_body::NoteBlock) -> Self {
        match b {
            note_body::NoteBlock::Paragraph { inline } => Self::Paragraph {
                inline: inline.into_iter().map(NoteInline::from).collect(),
            },
            note_body::NoteBlock::Heading { level, inline } => Self::Heading {
                level: level.into(),
                inline: inline.into_iter().map(NoteInline::from).collect(),
            },
            note_body::NoteBlock::List { ordered, items } => Self::List {
                ordered,
                items: items.into_iter().map(NoteListItem::from).collect(),
            },
            note_body::NoteBlock::Checklist { items } => Self::Checklist {
                items: items.into_iter().map(NoteChecklistItem::from).collect(),
            },
            note_body::NoteBlock::Code { language, content } => Self::Code { language, content },
            note_body::NoteBlock::Quote { inline } => Self::Quote {
                inline: inline.into_iter().map(NoteInline::from).collect(),
            },
            note_body::NoteBlock::Divider => Self::Divider,
        }
    }
}

impl From<NoteBlock> for note_body::NoteBlock {
    fn from(b: NoteBlock) -> Self {
        match b {
            NoteBlock::Paragraph { inline } => Self::Paragraph {
                inline: inline.into_iter().map(note_body::Inline::from).collect(),
            },
            NoteBlock::Heading { level, inline } => Self::Heading {
                level: level.into(),
                inline: inline.into_iter().map(note_body::Inline::from).collect(),
            },
            NoteBlock::List { ordered, items } => Self::List {
                ordered,
                items: items.into_iter().map(note_body::ListItem::from).collect(),
            },
            NoteBlock::Checklist { items } => Self::Checklist {
                items: items
                    .into_iter()
                    .map(note_body::ChecklistItem::from)
                    .collect(),
            },
            NoteBlock::Code { language, content } => Self::Code { language, content },
            NoteBlock::Quote { inline } => Self::Quote {
                inline: inline.into_iter().map(note_body::Inline::from).collect(),
            },
            NoteBlock::Divider => Self::Divider,
        }
    }
}

// ---------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------

/// See [`sunrise_domain::note_body::Fidelity`] — whether this build
/// understood a body completely enough to write it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum NoteFidelity {
    /// Re-encoding the blocks reproduces the body exactly. Safe to edit and
    /// save.
    Exact,
    /// The body held something this build could not put back. Render it;
    /// **do not save over it**.
    Lossy,
}

impl From<note_body::Fidelity> for NoteFidelity {
    fn from(f: note_body::Fidelity) -> Self {
        match f {
            note_body::Fidelity::Exact => Self::Exact,
            note_body::Fidelity::Lossy => Self::Lossy,
        }
    }
}

/// What [`decode_note_body`] made of a body.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NoteDocument {
    /// The best rendering this build can manage. Never an error, possibly
    /// empty.
    pub blocks: Vec<NoteBlock>,
    /// Whether [`blocks`](Self::blocks) may be written back over the input.
    pub fidelity: NoteFidelity,
}

impl From<note_body::NoteDoc> for NoteDocument {
    fn from(doc: note_body::NoteDoc) -> Self {
        let note_body::NoteDoc { blocks, fidelity } = doc;
        Self {
            blocks: blocks.into_iter().map(NoteBlock::from).collect(),
            fidelity: fidelity.into(),
        }
    }
}

/// The length limits `docs/02-domain/notes.md` §Length states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct NoteBodyLimits {
    /// Past this, performance degrades; the UI should nudge toward splitting.
    pub soft_bytes: u64,
    /// The ceiling.
    pub max_bytes: u64,
}

// ---------------------------------------------------------------------------
// Exports
// ---------------------------------------------------------------------------

/// Decode a body into blocks. **Never fails**, by contract.
///
/// A body that is not the grammar — plain text from the `sunrise` CLI, a
/// block kind from a newer build, a corrupt row — still renders as well as
/// this build can manage, and comes back marked [`NoteFidelity::Lossy`].
/// Check that before saving: see the module docs.
#[uniffi::export]
#[must_use]
pub fn decode_note_body(body: NoteBody) -> NoteDocument {
    note_body::decode(&body).into()
}

/// Encode blocks to canonical body bytes.
///
/// Canonical, so two clients that build the same document produce the same
/// bytes — which is what lets [`decode_note_body`] report fidelity as a byte
/// comparison rather than a guess.
#[uniffi::export]
#[must_use]
pub fn encode_note_body(blocks: Vec<NoteBlock>) -> NoteBody {
    let blocks: Vec<note_body::NoteBlock> =
        blocks.into_iter().map(note_body::NoteBlock::from).collect();
    note_body::encode(&blocks)
}

/// Export blocks to Markdown.
///
/// `docs/02-domain/notes.md`: "We export to Markdown; we don't store as
/// Markdown." One-way on purpose — there is no Markdown import, because
/// accepting Markdown back would make the stored grammar the loser of every
/// round trip.
#[uniffi::export]
#[must_use]
pub fn note_body_markdown(blocks: Vec<NoteBlock>) -> String {
    let blocks: Vec<note_body::NoteBlock> =
        blocks.into_iter().map(note_body::NoteBlock::from).collect();
    note_body::to_markdown(&blocks)
}

/// The spec's length limits, so a client does not restate them.
#[uniffi::export]
#[must_use]
pub fn note_body_limits() -> NoteBodyLimits {
    NoteBodyLimits {
        soft_bytes: note_body::NOTE_BODY_SOFT_LIMIT_BYTES as u64,
        max_bytes: note_body::NOTE_BODY_MAX_BYTES as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<NoteBlock> {
        vec![
            NoteBlock::Heading {
                level: NoteHeadingLevel::Two,
                inline: vec![NoteInline::Text {
                    text: "Packing".into(),
                    marks: vec![NoteMark::Bold],
                }],
            },
            NoteBlock::Checklist {
                items: vec![NoteChecklistItem {
                    checked: true,
                    inline: vec![NoteInline::Text {
                        text: "passport".into(),
                        marks: Vec::new(),
                    }],
                }],
            },
            NoteBlock::List {
                ordered: false,
                items: vec![NoteListItem {
                    inline: vec![NoteInline::Link {
                        href: "https://example.com".into(),
                        label: "booking".into(),
                    }],
                    children: vec![NoteBlock::Divider],
                }],
            },
            NoteBlock::Code {
                language: Some("sh".into()),
                content: "echo hi".into(),
            },
            NoteBlock::Quote {
                inline: vec![NoteInline::Ref {
                    target: "tsk_01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                }],
            },
        ]
    }

    #[test]
    fn the_seam_round_trips_a_document_without_touching_cbor_in_swift() {
        let body = encode_note_body(sample());
        let doc = decode_note_body(body);
        assert_eq!(doc.blocks, sample());
        assert_eq!(doc.fidelity, NoteFidelity::Exact);
    }

    #[test]
    fn a_body_the_grammar_cannot_name_crosses_as_lossy_rather_than_as_an_error() {
        // What the CLI writes. The seam has no error channel for this on
        // purpose: a client that got a thrown error here would show an empty
        // editor over a body that still has words in it.
        let doc = decode_note_body(NoteBody(b"remember the kangaroo".to_vec()));
        assert_eq!(doc.fidelity, NoteFidelity::Lossy);
        assert_eq!(
            doc.blocks,
            vec![NoteBlock::Paragraph {
                inline: vec![NoteInline::Text {
                    text: "remember the kangaroo".into(),
                    marks: Vec::new(),
                }]
            }]
        );
    }

    #[test]
    fn an_absent_body_crosses_as_an_empty_editable_document() {
        let doc = decode_note_body(NoteBody::empty());
        assert!(doc.blocks.is_empty());
        assert_eq!(doc.fidelity, NoteFidelity::Exact);
    }

    #[test]
    fn markdown_export_crosses_the_seam() {
        let md = note_body_markdown(sample());
        assert!(md.starts_with("## **Packing**"), "{md}");
        assert!(md.contains("- [x] passport"), "{md}");
        assert!(md.contains("```sh\necho hi\n```"), "{md}");
    }

    #[test]
    fn the_limits_are_the_domains_and_are_not_restated_here() {
        let limits = note_body_limits();
        assert_eq!(limits.soft_bytes, 65_536);
        assert_eq!(limits.max_bytes, 1_048_576);
    }
}

//! The `NoteBody` block grammar: decode, encode, and export to Markdown.
//!
//! Implements `docs/02-domain/notes.md` §Body grammar. Read the distinction
//! that file opens with, because the whole shape of this module follows from
//! it:
//!
//! > `NoteBody` is an **opaque byte string** on the wire. The core neither
//! > parses nor validates its contents; the grammar below is the contract
//! > editors and renderers agree on *inside* those bytes, not a shape the
//! > CBOR codec enforces.
//!
//! So this module is a **renderer contract**, not a validator. Two
//! consequences it is built around:
//!
//! 1. **Decoding cannot fail.** [`decode`] is total. A body that is not CBOR,
//!    or is CBOR the grammar does not name, still produces the best rendering
//!    this build can manage — down to plain UTF-8 lines as paragraphs, which
//!    is exactly what the `sunrise` CLI writes ("plain text only; no
//!    structured-body editing"). The failure mode is "render as best you
//!    can", never "drop the bytes".
//! 2. **Adding a block kind is not a `DOC_SCHEMA_V` change**, so an older
//!    build meeting a newer body must not corrupt it. That is what
//!    [`Fidelity`] is for.
//!
//! # Fidelity is the safety interlock
//!
//! [`decode`] reports [`Fidelity::Exact`] only when re-encoding the blocks it
//! produced reproduces the input bytes *exactly*. Anything else — an unknown
//! block kind, an unknown mark, a key this build does not model, a body that
//! was never CBOR — is [`Fidelity::Lossy`].
//!
//! That single rule is unfoolable, because it does not enumerate the ways a
//! body can be strange; it just checks whether this build can put it back.
//! A structured editor **must not save over a `Lossy` body**: doing so would
//! silently drop whatever it could not name. It should render what it got,
//! and leave the original bytes alone.
//!
//! The rule has exactly one documented exception: an **absent** body — no
//! bytes at all — is the same document as an **empty** one (`[]`, a single
//! `0x80`), so it is [`Fidelity::Exact`] even though re-encoding picks the
//! latter spelling. Both mean "no content", and which of the two a field
//! holds is `clear_body`'s business, not the codec's.
//!
//! # Merge
//!
//! A `NoteBody` is one last-writer-wins unit on `(hlc, device_id, seq)`
//! (ADR-0014). Two devices editing one body produce **one survivor, not a
//! character-level merge**; this workspace ships no text CRDT. Nothing in
//! this module changes that, and no editor built on it should imply
//! otherwise.

use crate::common::NoteBody;
use ciborium::value::Value;

/// Soft length limit for one body, per `docs/02-domain/notes.md` §Length.
/// Past this, performance degrades and the UI should nudge toward splitting.
pub const NOTE_BODY_SOFT_LIMIT_BYTES: usize = 64 * 1024;

/// Hard length limit for one body, per `docs/02-domain/notes.md` §Length.
pub const NOTE_BODY_MAX_BYTES: usize = 1024 * 1024;

/// How deep list nesting may go before [`decode`] stops descending.
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
const MAX_PLAIN_TEXT_PARAGRAPHS: usize = 4096;

/// An empty document: CBOR `[]`.
const EMPTY_DOC: [u8; 1] = [0x80];

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
        /// [`encode`] so that two editors that chose different orders produce
        /// the same bytes.
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

/// What [`decode`] made of a body.
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

// ---------------------------------------------------------------------------
// Encode
// ---------------------------------------------------------------------------

/// Encode blocks to canonical CBOR body bytes.
///
/// Canonical, via [`sunrise_cbor::encode_canonical`]: map keys sorted by their
/// encoded bytes, definite lengths, shortest-form integers. Two editors that
/// build the same document therefore produce the same bytes, which is what
/// makes [`Fidelity`] a byte comparison rather than a guess.
#[must_use]
pub fn encode(blocks: &[NoteBlock]) -> NoteBody {
    let value = Value::Array(blocks.iter().map(block_to_value).collect());
    if let Ok(bytes) = sunrise_cbor::encode_canonical(&value) {
        return NoteBody(bytes);
    }
    // Unreachable: `encode_canonical` fails on a value CBOR cannot represent
    // — a float, or a type ciborium refuses — and every `Value` built below
    // is an array, map, text, integer or bool.
    debug_assert!(false, "note body values are always CBOR-encodable");
    NoteBody(EMPTY_DOC.to_vec())
}

fn key(name: &str) -> Value {
    Value::Text(name.to_string())
}

fn inline_seq(inline: &[Inline]) -> Value {
    Value::Array(inline.iter().map(inline_to_value).collect())
}

fn block_to_value(block: &NoteBlock) -> Value {
    let entries = match block {
        NoteBlock::Paragraph { inline } => {
            vec![(key("kind"), key("p")), (key("inline"), inline_seq(inline))]
        }
        NoteBlock::Heading { level, inline } => vec![
            (key("kind"), key("h")),
            (key("level"), Value::Integer(level.as_u8().into())),
            (key("inline"), inline_seq(inline)),
        ],
        NoteBlock::List { ordered, items } => vec![
            (key("kind"), key(if *ordered { "ol" } else { "ul" })),
            (
                key("items"),
                Value::Array(items.iter().map(list_item_to_value).collect()),
            ),
        ],
        NoteBlock::Checklist { items } => vec![
            (key("kind"), key("task")),
            (
                key("items"),
                Value::Array(
                    items
                        .iter()
                        .map(|it| {
                            Value::Map(vec![
                                (key("checked"), Value::Bool(it.checked)),
                                (key("inline"), inline_seq(&it.inline)),
                            ])
                        })
                        .collect(),
                ),
            ),
        ],
        NoteBlock::Code { language, content } => {
            let mut entries = vec![(key("kind"), key("code")), (key("content"), key(content))];
            // `language?` — omitted, not null, when absent. A `language:
            // null` would be a different encoding of the same document and
            // would make every such body read as lossy.
            if let Some(language) = language {
                entries.push((key("language"), key(language)));
            }
            entries
        }
        NoteBlock::Quote { inline } => vec![
            (key("kind"), key("quote")),
            (key("inline"), inline_seq(inline)),
        ],
        NoteBlock::Divider => vec![(key("kind"), key("hr"))],
    };
    Value::Map(entries)
}

fn list_item_to_value(item: &ListItem) -> Value {
    let mut entries = vec![(key("inline"), inline_seq(&item.inline))];
    if !item.children.is_empty() {
        entries.push((
            key("children"),
            Value::Array(item.children.iter().map(block_to_value).collect()),
        ));
    }
    Value::Map(entries)
}

fn inline_to_value(inline: &Inline) -> Value {
    let entries = match inline {
        Inline::Text { text, marks } => {
            let mut entries = vec![(key("text"), key(text))];
            if !marks.is_empty() {
                // Sorted and deduplicated: `[bold, italic]` and
                // `[italic, bold, bold]` are the same run, and two editors
                // that disagreed on the order would each read the other's
                // bodies as lossy.
                let mut marks = marks.clone();
                marks.sort_unstable();
                marks.dedup();
                entries.push((
                    key("marks"),
                    Value::Array(marks.into_iter().map(|m| key(m.as_str())).collect()),
                ));
            }
            entries
        }
        Inline::Link { href, label } => vec![
            (key("kind"), key("link")),
            (key("href"), key(href)),
            (key("label"), key(label)),
        ],
        Inline::Ref { target } => vec![(key("kind"), key("ref")), (key("ref"), key(target))],
        Inline::Mention { person } => {
            vec![(key("kind"), key("mention")), (key("person"), key(person))]
        }
        Inline::Redacted {
            reason,
            placeholder_text,
        } => vec![
            (key("kind"), key("redacted")),
            (key("reason"), key(reason)),
            (key("placeholder_text"), key(placeholder_text)),
        ],
    };
    Value::Map(entries)
}

// ---------------------------------------------------------------------------
// Decode
// ---------------------------------------------------------------------------

/// Decode a body into blocks. **Never fails.**
///
/// The ladder, in order:
///
/// 1. Empty body → an empty document, [`Fidelity::Exact`]. An absent body is
///    not a malformed one.
/// 2. CBOR array → each element decoded as a block; elements the grammar does
///    not name are skipped.
/// 3. Valid UTF-8 that is not a CBOR array → one paragraph per non-blank
///    line. This is the `sunrise` CLI's plain-text body, and a note editor
///    that showed nothing for one would look broken.
/// 4. Anything else → no blocks.
///
/// Steps 3 and 4 are always [`Fidelity::Lossy`], as is any step-2 body this
/// build cannot re-encode byte-for-byte. See the module docs: lossy means
/// *render, do not overwrite*.
#[must_use]
pub fn decode(body: &NoteBody) -> NoteDoc {
    if body.is_empty() {
        return NoteDoc {
            blocks: Vec::new(),
            fidelity: Fidelity::Exact,
        };
    }

    let blocks = match ciborium::de::from_reader::<Value, _>(body.0.as_slice()) {
        Ok(Value::Array(items)) => items
            .iter()
            .filter_map(|item| block_from_value(item, 0))
            .collect(),
        // Decoded to something that is not a document, or did not decode at
        // all. Either way the bytes are not this grammar; fall back to text.
        Ok(_) | Err(_) => plain_text_blocks(&body.0),
    };

    // The one rule: can this build put back exactly what it was given?
    let fidelity = if encode(&blocks).0 == body.0 {
        Fidelity::Exact
    } else {
        Fidelity::Lossy
    };
    NoteDoc { blocks, fidelity }
}

/// Render bytes that are not the grammar as paragraphs, if they are text.
fn plain_text_blocks(bytes: &[u8]) -> Vec<NoteBlock> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .take(MAX_PLAIN_TEXT_PARAGRAPHS)
        .map(|line| NoteBlock::Paragraph {
            inline: vec![Inline::text(line)],
        })
        .collect()
}

fn entry<'a>(map: &'a [(Value, Value)], name: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_text() == Some(name))
        .map(|(_, v)| v)
}

fn text_at(map: &[(Value, Value)], name: &str) -> Option<String> {
    entry(map, name).and_then(Value::as_text).map(str::to_owned)
}

fn inline_at(map: &[(Value, Value)], name: &str) -> Vec<Inline> {
    entry(map, name)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(inline_from_value).collect())
        .unwrap_or_default()
}

fn block_from_value(value: &Value, depth: usize) -> Option<NoteBlock> {
    let map = value.as_map()?;
    let kind = entry(map, "kind")?.as_text()?;
    Some(match kind {
        "p" => NoteBlock::Paragraph {
            inline: inline_at(map, "inline"),
        },
        "h" => NoteBlock::Heading {
            level: entry(map, "level")
                .and_then(Value::as_integer)
                .and_then(|i| i64::try_from(i).ok())
                .map_or(HeadingLevel::One, HeadingLevel::clamp_from),
            inline: inline_at(map, "inline"),
        },
        "ul" | "ol" => NoteBlock::List {
            ordered: kind == "ol",
            items: entry(map, "items")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| list_item_from_value(item, depth))
                        .collect()
                })
                .unwrap_or_default(),
        },
        "task" => NoteBlock::Checklist {
            items: entry(map, "items")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(checklist_item_from_value).collect())
                .unwrap_or_default(),
        },
        "code" => NoteBlock::Code {
            language: text_at(map, "language"),
            content: text_at(map, "content").unwrap_or_default(),
        },
        "quote" => NoteBlock::Quote {
            inline: inline_at(map, "inline"),
        },
        "hr" => NoteBlock::Divider,
        // A kind from a newer build. Skipping it is what makes the body
        // lossy, which is what stops this build overwriting it.
        _ => return None,
    })
}

fn list_item_from_value(value: &Value, depth: usize) -> Option<ListItem> {
    let map = value.as_map()?;
    // Past the floor the children are dropped rather than followed. The body
    // becomes lossy and therefore read-only; the alternative is a stack
    // overflow on a body someone else wrote.
    let children = if depth + 1 >= MAX_NEST_DEPTH {
        Vec::new()
    } else {
        entry(map, "children")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| block_from_value(item, depth + 1))
                    .collect()
            })
            .unwrap_or_default()
    };
    Some(ListItem {
        inline: inline_at(map, "inline"),
        children,
    })
}

fn checklist_item_from_value(value: &Value) -> Option<ChecklistItem> {
    let map = value.as_map()?;
    Some(ChecklistItem {
        checked: entry(map, "checked")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        inline: inline_at(map, "inline"),
    })
}

fn inline_from_value(value: &Value) -> Option<Inline> {
    let map = value.as_map()?;
    // No `kind` is the text form: `{text: text, marks?: [* Mark]}` is the one
    // Inline alternative the grammar writes without a discriminant.
    let Some(kind) = entry(map, "kind").and_then(Value::as_text) else {
        return Some(Inline::Text {
            text: text_at(map, "text")?,
            marks: entry(map, "marks")
                .and_then(Value::as_array)
                .map(|marks| {
                    marks
                        .iter()
                        .filter_map(Value::as_text)
                        .filter_map(Mark::from_wire)
                        .collect()
                })
                .unwrap_or_default(),
        });
    };
    Some(match kind {
        "link" => Inline::Link {
            href: text_at(map, "href")?,
            label: text_at(map, "label")?,
        },
        "ref" => Inline::Ref {
            target: text_at(map, "ref")?,
        },
        "mention" => Inline::Mention {
            person: text_at(map, "person")?,
        },
        "redacted" => Inline::Redacted {
            reason: text_at(map, "reason").unwrap_or_default(),
            placeholder_text: text_at(map, "placeholder_text").unwrap_or_default(),
        },
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Markdown export
// ---------------------------------------------------------------------------

/// Export blocks to Markdown.
///
/// `docs/02-domain/notes.md`: "We export to Markdown; we don't store as
/// Markdown." This is the export half. It is deliberately one-way — there is
/// no Markdown *import*, because accepting Markdown back would make the
/// stored grammar the loser of every round trip.
///
/// Two places Markdown cannot say what the grammar says, and what is emitted
/// instead:
///
/// * **Underline** has no Markdown syntax, so it emits `<u>…</u>`.
/// * **Refs and mentions** render as the target's title in-app, and this
///   crate does not know titles. They emit the id in a code span; a redacted
///   inline emits its placeholder text.
#[must_use]
pub fn to_markdown(blocks: &[NoteBlock]) -> String {
    let mut out = String::new();
    write_blocks(&mut out, blocks, 0);
    // One trailing newline, however many blank lines the blocks left behind.
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

fn write_blocks(out: &mut String, blocks: &[NoteBlock], indent: usize) {
    for block in blocks {
        write_block(out, block, indent);
    }
}

fn pad(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("    ");
    }
}

fn write_block(out: &mut String, block: &NoteBlock, indent: usize) {
    match block {
        NoteBlock::Paragraph { inline } => {
            pad(out, indent);
            out.push_str(&inline_markdown(inline));
            out.push_str("\n\n");
        }
        NoteBlock::Heading { level, inline } => {
            pad(out, indent);
            for _ in 0..level.as_u8() {
                out.push('#');
            }
            out.push(' ');
            out.push_str(&inline_markdown(inline));
            out.push_str("\n\n");
        }
        NoteBlock::List { ordered, items } => {
            for (i, item) in items.iter().enumerate() {
                pad(out, indent);
                if *ordered {
                    out.push_str(&(i + 1).to_string());
                    out.push_str(". ");
                } else {
                    out.push_str("- ");
                }
                out.push_str(&inline_markdown(&item.inline));
                out.push('\n');
                write_blocks(out, &item.children, indent + 1);
            }
            out.push('\n');
        }
        NoteBlock::Checklist { items } => {
            for item in items {
                pad(out, indent);
                out.push_str(if item.checked { "- [x] " } else { "- [ ] " });
                out.push_str(&inline_markdown(&item.inline));
                out.push('\n');
            }
            out.push('\n');
        }
        NoteBlock::Code { language, content } => {
            pad(out, indent);
            out.push_str("```");
            if let Some(language) = language {
                out.push_str(language);
            }
            out.push('\n');
            out.push_str(content);
            if !content.ends_with('\n') {
                out.push('\n');
            }
            pad(out, indent);
            out.push_str("```\n\n");
        }
        NoteBlock::Quote { inline } => {
            pad(out, indent);
            out.push_str("> ");
            out.push_str(&inline_markdown(inline));
            out.push_str("\n\n");
        }
        NoteBlock::Divider => {
            pad(out, indent);
            out.push_str("---\n\n");
        }
    }
}

fn inline_markdown(inline: &[Inline]) -> String {
    let mut out = String::new();
    for run in inline {
        match run {
            Inline::Text { text, marks } => out.push_str(&marked(text, marks)),
            Inline::Link { href, label } => {
                out.push('[');
                out.push_str(&escape(label));
                out.push_str("](");
                out.push_str(href);
                out.push(')');
            }
            Inline::Ref { target } => {
                out.push('`');
                out.push_str(target);
                out.push('`');
            }
            Inline::Mention { person } => {
                out.push_str("`@");
                out.push_str(person);
                out.push('`');
            }
            Inline::Redacted {
                placeholder_text, ..
            } => out.push_str(&escape(placeholder_text)),
        }
    }
    out
}

/// Wrap `text` in its marks. `code` is innermost and suppresses escaping,
/// because a code span is literal by definition.
fn marked(text: &str, marks: &[Mark]) -> String {
    let has = |m: Mark| marks.contains(&m);
    let mut out = if has(Mark::Code) {
        format!("`{text}`")
    } else {
        escape(text)
    };
    if has(Mark::Strike) {
        out = format!("~~{out}~~");
    }
    if has(Mark::Underline) {
        out = format!("<u>{out}</u>");
    }
    if has(Mark::Italic) {
        out = format!("*{out}*");
    }
    if has(Mark::Bold) {
        out = format!("**{out}**");
    }
    out
}

/// Escape the characters that would otherwise be read as markup.
///
/// A conservative set. Over-escaping produces Markdown nobody wants to read,
/// and these five are the ones that change meaning mid-line.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '\\' | '*' | '_' | '[' | ']' | '`') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(blocks: &[NoteBlock]) {
        let body = encode(blocks);
        let doc = decode(&body);
        assert_eq!(doc.blocks, blocks, "blocks changed across a round trip");
        assert_eq!(
            doc.fidelity,
            Fidelity::Exact,
            "a body this build wrote must read back as exact"
        );
        assert_eq!(encode(&doc.blocks).0, body.0, "bytes are not stable");
    }

    fn marked_text(text: &str, marks: &[Mark]) -> Inline {
        Inline::Text {
            text: text.to_string(),
            marks: marks.to_vec(),
        }
    }

    // -- CBOR fixtures ------------------------------------------------------
    //
    // Every test below asserts `decode`'s **public** contract, so none of them
    // may be built out of the encoder's internals: a fixture spelled with the
    // private `key` helper reads as a test of `encode`'s guts, and moves
    // whenever they do. These build the bodies a *peer* writes — an unknown
    // block kind, an unknown mark, a key this build does not model — which the
    // public API deliberately cannot express, so a hand-built value is the
    // only way to reach them. `tests/note_body_proptest.rs` makes the same
    // choice from outside the crate, and for the same reason.

    /// CBOR `[]`: the explicitly empty document a conforming peer writes.
    /// Spelled here rather than read off the encoder's private `EMPTY_DOC`,
    /// because what is being asserted is that *this byte* decodes as empty.
    const EMPTY_DOC_BYTES: [u8; 1] = [0x80];

    fn cbor_text(s: &str) -> ciborium::value::Value {
        ciborium::value::Value::Text(s.to_string())
    }

    fn cbor_int(n: i64) -> ciborium::value::Value {
        ciborium::value::Value::Integer(n.into())
    }

    fn cbor_array(items: Vec<ciborium::value::Value>) -> ciborium::value::Value {
        ciborium::value::Value::Array(items)
    }

    /// A CBOR map with text keys. Order is irrelevant — `encode_canonical`
    /// sorts them — so these read in grammar order.
    fn cbor_map(entries: Vec<(&str, ciborium::value::Value)>) -> ciborium::value::Value {
        ciborium::value::Value::Map(
            entries
                .into_iter()
                .map(|(k, v)| (cbor_text(k), v))
                .collect(),
        )
    }

    /// Canonical CBOR bytes for `value`, as a body ready to hand to `decode`.
    fn cbor_body(value: &ciborium::value::Value) -> NoteBody {
        NoteBody(sunrise_cbor::encode_canonical(value).expect("fixture encodes"))
    }

    #[test]
    fn an_absent_body_is_an_empty_document_not_a_broken_one() {
        let doc = decode(&NoteBody::empty());
        assert!(doc.blocks.is_empty());
        assert!(doc.is_exact(), "nothing to lose means nothing was lost");
    }

    #[test]
    fn an_explicitly_empty_document_is_exact_too() {
        // `[]` — what a conforming peer writes for a body with no blocks. It
        // is not the same *bytes* as an absent body, and marking it lossy
        // would make every emptied note read-only.
        let doc = decode(&NoteBody(EMPTY_DOC_BYTES.to_vec()));
        assert!(doc.blocks.is_empty());
        assert!(doc.is_exact());
        assert_eq!(encode(&[]).0, EMPTY_DOC_BYTES.to_vec());
    }

    #[test]
    fn every_block_kind_round_trips() {
        round_trip(&[
            NoteBlock::Paragraph {
                inline: vec![Inline::text("plain")],
            },
            NoteBlock::Heading {
                level: HeadingLevel::One,
                inline: vec![Inline::text("one")],
            },
            NoteBlock::Heading {
                level: HeadingLevel::Two,
                inline: vec![Inline::text("two")],
            },
            NoteBlock::Heading {
                level: HeadingLevel::Three,
                inline: vec![Inline::text("three")],
            },
            NoteBlock::List {
                ordered: false,
                items: vec![ListItem {
                    inline: vec![Inline::text("bullet")],
                    children: Vec::new(),
                }],
            },
            NoteBlock::List {
                ordered: true,
                items: vec![ListItem {
                    inline: vec![Inline::text("numbered")],
                    children: Vec::new(),
                }],
            },
            NoteBlock::Checklist {
                items: vec![
                    ChecklistItem {
                        checked: true,
                        inline: vec![Inline::text("done")],
                    },
                    ChecklistItem {
                        checked: false,
                        inline: vec![Inline::text("not done")],
                    },
                ],
            },
            NoteBlock::Code {
                language: Some("rust".into()),
                content: "fn main() {}".into(),
            },
            NoteBlock::Code {
                language: None,
                content: "no language".into(),
            },
            NoteBlock::Quote {
                inline: vec![Inline::text("quoted")],
            },
            NoteBlock::Divider,
        ]);
    }

    #[test]
    fn every_mark_round_trips_alone_and_together() {
        let mut runs: Vec<Inline> = Mark::ALL
            .iter()
            .map(|m| marked_text("run", &[*m]))
            .collect();
        runs.push(marked_text("all of them", &Mark::ALL));
        round_trip(&[NoteBlock::Paragraph { inline: runs }]);
    }

    #[test]
    fn every_inline_kind_round_trips() {
        round_trip(&[NoteBlock::Paragraph {
            inline: vec![
                Inline::text("text"),
                Inline::Link {
                    href: "https://example.com".into(),
                    label: "a link".into(),
                },
                Inline::Ref {
                    target: "tsk_01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                },
                Inline::Mention {
                    person: "per_01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                },
                Inline::Redacted {
                    reason: "private_ref".into(),
                    placeholder_text: "(redacted)".into(),
                },
            ],
        }]);
    }

    #[test]
    fn nested_list_children_round_trip() {
        round_trip(&[NoteBlock::List {
            ordered: false,
            items: vec![ListItem {
                inline: vec![Inline::text("outer")],
                children: vec![NoteBlock::List {
                    ordered: true,
                    items: vec![ListItem {
                        inline: vec![Inline::text("inner")],
                        children: vec![NoteBlock::Paragraph {
                            inline: vec![Inline::text("deepest")],
                        }],
                    }],
                }],
            }],
        }]);
    }

    #[test]
    fn marks_are_sorted_and_deduplicated_so_two_editors_agree() {
        let a = encode(&[NoteBlock::Paragraph {
            inline: vec![marked_text("x", &[Mark::Italic, Mark::Bold])],
        }]);
        let b = encode(&[NoteBlock::Paragraph {
            inline: vec![marked_text("x", &[Mark::Bold, Mark::Italic, Mark::Bold])],
        }]);
        assert_eq!(a.0, b.0, "mark order must not change the bytes");
    }

    #[test]
    fn an_empty_mark_list_is_omitted_rather_than_written_empty() {
        let with_empty = encode(&[NoteBlock::Paragraph {
            inline: vec![marked_text("x", &[])],
        }]);
        let without = encode(&[NoteBlock::Paragraph {
            inline: vec![Inline::text("x")],
        }]);
        assert_eq!(with_empty.0, without.0);
    }

    // -- Malformed input: the whole point of the module ---------------------

    #[test]
    fn bytes_that_are_not_cbor_render_as_paragraphs_and_are_never_dropped() {
        // What `sunrise` CLI writes: plain text, no structured editing.
        let body = NoteBody(b"remember the kangaroo\nand the emu".to_vec());
        let doc = decode(&body);
        assert_eq!(
            doc.blocks,
            vec![
                NoteBlock::Paragraph {
                    inline: vec![Inline::text("remember the kangaroo")]
                },
                NoteBlock::Paragraph {
                    inline: vec![Inline::text("and the emu")]
                },
            ],
            "a plain-text body must render, not vanish"
        );
        assert_eq!(
            doc.fidelity,
            Fidelity::Lossy,
            "it is not the grammar, so it must not be written back over"
        );
        // The bytes the caller holds are untouched — this is the hard
        // requirement: the codec never consumes a body it cannot express.
        assert_eq!(body.0, b"remember the kangaroo\nand the emu".to_vec());
    }

    #[test]
    fn bytes_that_are_neither_cbor_nor_utf8_still_decode_to_something() {
        let doc = decode(&NoteBody(vec![0xff, 0xfe, 0xfd, 0x00, 0x9c]));
        assert!(doc.blocks.is_empty(), "nothing renderable was found");
        assert_eq!(doc.fidelity, Fidelity::Lossy, "so nothing may be saved");
    }

    #[test]
    fn a_truncated_body_does_not_panic() {
        let full = encode(&[NoteBlock::Paragraph {
            inline: vec![Inline::text("hello")],
        }]);
        for cut in 1..full.0.len() {
            let doc = decode(&NoteBody(full.0[..cut].to_vec()));
            assert_eq!(doc.fidelity, Fidelity::Lossy, "cut at {cut}");
        }
    }

    #[test]
    fn a_block_kind_from_a_newer_build_is_skipped_and_locks_the_body() {
        let body = cbor_body(&cbor_array(vec![
            cbor_map(vec![
                ("kind", cbor_text("p")),
                ("inline", cbor_array(vec![])),
            ]),
            // Something this build has never heard of.
            cbor_map(vec![("kind", cbor_text("gantt"))]),
        ]));
        let doc = decode(&body);
        assert_eq!(
            doc.blocks,
            vec![NoteBlock::Paragraph { inline: Vec::new() }],
            "the block it understood still renders"
        );
        assert_eq!(
            doc.fidelity,
            Fidelity::Lossy,
            "and the one it did not is what stops the save"
        );
    }

    #[test]
    fn an_unknown_mark_is_dropped_from_the_render_and_locks_the_body() {
        let body = cbor_body(&cbor_array(vec![cbor_map(vec![
            ("kind", cbor_text("p")),
            (
                "inline",
                cbor_array(vec![cbor_map(vec![
                    ("text", cbor_text("x")),
                    (
                        "marks",
                        cbor_array(vec![cbor_text("bold"), cbor_text("sparkle")]),
                    ),
                ])]),
            ),
        ])]));
        let doc = decode(&body);
        assert_eq!(
            doc.blocks,
            vec![NoteBlock::Paragraph {
                inline: vec![marked_text("x", &[Mark::Bold])]
            }]
        );
        assert_eq!(doc.fidelity, Fidelity::Lossy);
    }

    #[test]
    fn a_heading_level_outside_the_grammar_is_clamped_not_refused() {
        for (written, expected) in [
            (0i64, HeadingLevel::One),
            (1, HeadingLevel::One),
            (2, HeadingLevel::Two),
            (3, HeadingLevel::Three),
            (6, HeadingLevel::Three),
            (-9, HeadingLevel::One),
        ] {
            let body = cbor_body(&cbor_array(vec![cbor_map(vec![
                ("kind", cbor_text("h")),
                ("level", cbor_int(written)),
                ("inline", cbor_array(vec![])),
            ])]));
            let doc = decode(&body);
            assert_eq!(
                doc.blocks,
                vec![NoteBlock::Heading {
                    level: expected,
                    inline: Vec::new()
                }],
                "level {written}"
            );
        }
    }

    #[test]
    fn a_field_this_build_does_not_model_makes_the_body_read_only() {
        let body = cbor_body(&cbor_array(vec![cbor_map(vec![
            ("kind", cbor_text("p")),
            ("inline", cbor_array(vec![])),
            ("alignment", cbor_text("centre")),
        ])]));
        let doc = decode(&body);
        assert_eq!(
            doc.blocks,
            vec![NoteBlock::Paragraph { inline: Vec::new() }]
        );
        assert_eq!(
            doc.fidelity,
            Fidelity::Lossy,
            "an unmodelled key would be dropped by a save, so saving is refused"
        );
    }

    #[test]
    fn a_top_level_value_that_is_not_a_document_falls_back_to_text() {
        // CBOR that decodes fine but is not `[* NoteBlock]`.
        let body = cbor_body(&cbor_text("just a string"));
        let doc = decode(&body);
        assert_eq!(doc.fidelity, Fidelity::Lossy);
    }

    #[test]
    fn nesting_past_the_floor_is_dropped_rather_than_overflowing_the_stack() {
        // Deeper than `MAX_NEST_DEPTH`, built from the inside out.
        let mut block = NoteBlock::Paragraph {
            inline: vec![Inline::text("bottom")],
        };
        for _ in 0..(MAX_NEST_DEPTH * 3) {
            block = NoteBlock::List {
                ordered: false,
                items: vec![ListItem {
                    inline: Vec::new(),
                    children: vec![block],
                }],
            };
        }
        let doc = decode(&encode(&[block]));
        assert_eq!(
            doc.fidelity,
            Fidelity::Lossy,
            "the dropped children make it read-only"
        );
        // And, crucially, it returned at all.
        assert_eq!(doc.blocks.len(), 1);
    }

    #[test]
    fn a_lossy_body_that_is_re_encoded_from_its_render_never_replaces_it() {
        // The interlock, stated as the invariant a caller relies on: for any
        // body, `decode` either says Exact (and re-encoding is a no-op) or
        // says Lossy (and the caller keeps the original bytes).
        for bytes in [
            b"plain text".to_vec(),
            vec![0x00, 0x01, 0x02],
            vec![0xff],
            encode(&[NoteBlock::Divider]).0,
        ] {
            let body = NoteBody(bytes.clone());
            let doc = decode(&body);
            if doc.is_exact() {
                assert_eq!(encode(&doc.blocks).0, bytes);
            }
        }
    }

    // -- Markdown export ----------------------------------------------------

    #[test]
    fn markdown_export_covers_every_block_kind() {
        let md = to_markdown(&[
            NoteBlock::Heading {
                level: HeadingLevel::Two,
                inline: vec![Inline::text("Title")],
            },
            NoteBlock::Paragraph {
                inline: vec![
                    marked_text("bold", &[Mark::Bold]),
                    Inline::text(" and "),
                    marked_text("italic", &[Mark::Italic]),
                ],
            },
            NoteBlock::List {
                ordered: false,
                items: vec![ListItem {
                    inline: vec![Inline::text("one")],
                    children: Vec::new(),
                }],
            },
            NoteBlock::List {
                ordered: true,
                items: vec![
                    ListItem {
                        inline: vec![Inline::text("first")],
                        children: Vec::new(),
                    },
                    ListItem {
                        inline: vec![Inline::text("second")],
                        children: Vec::new(),
                    },
                ],
            },
            NoteBlock::Checklist {
                items: vec![
                    ChecklistItem {
                        checked: true,
                        inline: vec![Inline::text("packed")],
                    },
                    ChecklistItem {
                        checked: false,
                        inline: vec![Inline::text("posted")],
                    },
                ],
            },
            NoteBlock::Code {
                language: Some("rust".into()),
                content: "fn main() {}".into(),
            },
            NoteBlock::Quote {
                inline: vec![Inline::text("quoted")],
            },
            NoteBlock::Divider,
        ]);
        assert_eq!(
            md,
            "## Title\n\
             \n\
             **bold** and *italic*\n\
             \n\
             - one\n\
             \n\
             1. first\n\
             2. second\n\
             \n\
             - [x] packed\n\
             - [ ] posted\n\
             \n\
             ```rust\n\
             fn main() {}\n\
             ```\n\
             \n\
             > quoted\n\
             \n\
             ---\n"
        );
    }

    #[test]
    fn markdown_export_indents_nested_list_children() {
        let md = to_markdown(&[NoteBlock::List {
            ordered: false,
            items: vec![ListItem {
                inline: vec![Inline::text("outer")],
                children: vec![NoteBlock::List {
                    ordered: false,
                    items: vec![ListItem {
                        inline: vec![Inline::text("inner")],
                        children: Vec::new(),
                    }],
                }],
            }],
        }]);
        assert_eq!(md, "- outer\n    - inner\n");
    }

    #[test]
    fn markdown_export_says_what_markdown_cannot() {
        let md = to_markdown(&[NoteBlock::Paragraph {
            inline: vec![
                marked_text("under", &[Mark::Underline]),
                Inline::Ref {
                    target: "tsk_01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                },
                Inline::Mention {
                    person: "per_01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
                },
                Inline::Redacted {
                    reason: "deleted_entity".into(),
                    placeholder_text: "(removed)".into(),
                },
            ],
        }]);
        assert_eq!(
            md,
            "<u>under</u>`tsk_01ARZ3NDEKTSV4RRFFQ69G5FAV``@per_01ARZ3NDEKTSV4RRFFQ69G5FAV`(removed)\n"
        );
    }

    #[test]
    fn markdown_export_escapes_characters_that_would_become_markup() {
        let md = to_markdown(&[NoteBlock::Paragraph {
            inline: vec![Inline::text("a *star* and _under_ and [bracket]")],
        }]);
        assert_eq!(md, "a \\*star\\* and \\_under\\_ and \\[bracket\\]\n");
    }

    #[test]
    fn a_code_mark_is_literal_and_is_not_escaped_inside_its_span() {
        let md = to_markdown(&[NoteBlock::Paragraph {
            inline: vec![marked_text("a_b*c", &[Mark::Code])],
        }]);
        assert_eq!(md, "`a_b*c`\n");
    }

    #[test]
    fn a_link_keeps_its_href_verbatim_and_escapes_only_its_label() {
        let md = to_markdown(&[NoteBlock::Paragraph {
            inline: vec![Inline::Link {
                href: "https://example.com/a_b".into(),
                label: "a_b".into(),
            }],
        }]);
        assert_eq!(md, "[a\\_b](https://example.com/a_b)\n");
    }

    #[test]
    fn the_spec_length_limits_are_the_ones_the_document_states() {
        assert_eq!(NOTE_BODY_SOFT_LIMIT_BYTES, 65_536);
        assert_eq!(NOTE_BODY_MAX_BYTES, 1_048_576);
    }
}

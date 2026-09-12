//! One direction: blocks to canonical CBOR body bytes.
//!
//! Everything here builds a [`Value`] and nothing here reads one, which is why
//! it is a module rather than a half of one. Canonicality is the entire
//! contract — sorted keys, definite lengths, shortest-form integers, marks
//! sorted and deduplicated, an absent optional omitted rather than written
//! null — because it is what lets [`super::Fidelity`] be a byte comparison
//! rather than a guess about what a peer meant.

use super::grammar::{Inline, ListItem, NoteBlock, EMPTY_DOC};
use crate::common::NoteBody;
use ciborium::value::Value;

// ---------------------------------------------------------------------------
// Encode
// ---------------------------------------------------------------------------

/// Encode blocks to canonical CBOR body bytes.
///
/// Canonical, via [`sunrise_cbor::encode_canonical`]: map keys sorted by their
/// encoded bytes, definite lengths, shortest-form integers. Two editors that
/// build the same document therefore produce the same bytes, which is what
/// makes [`Fidelity`](super::Fidelity) a byte comparison rather than a guess.
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

#[cfg(test)]
mod tests {
    use super::super::grammar::Mark;
    use super::*;

    fn marked_text(text: &str, marks: &[Mark]) -> Inline {
        Inline::Text {
            text: text.to_string(),
            marks: marks.to_vec(),
        }
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
}

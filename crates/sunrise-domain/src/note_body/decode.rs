//! The other direction: body bytes to blocks, plus the depth floor that stops
//! a body someone else wrote from overflowing the stack.
//!
//! [`decode`] computes its [`Fidelity`] by calling [`encode`]: a body is
//! [`Fidelity::Exact`] exactly when re-encoding what was decoded reproduces
//! the input bytes. That edge is the interlock the module docs describe, it
//! runs one way — decode to encode, never the reverse — and keeping the two
//! directions in separate modules is what makes the direction visible rather
//! than incidental.

use super::encode::encode;
use super::grammar::{
    ChecklistItem, Fidelity, HeadingLevel, Inline, ListItem, Mark, NoteBlock, NoteDoc,
    MAX_NEST_DEPTH, MAX_PLAIN_TEXT_PARAGRAPHS,
};
use crate::common::NoteBody;
use ciborium::value::Value;

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
///    that showed nothing for one would look broken. Capped at 4096
///    paragraphs (`MAX_PLAIN_TEXT_PARAGRAPHS`); lines past it are dropped.
///    The body is [`Fidelity::Lossy`] either way, so the original bytes are
///    kept — but a renderer showing a very long plain-text note shows a
///    prefix of it, with no marker at the cut.
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
                assert_eq!(
                    encode(&doc.blocks).0,
                    bytes,
                    "Exact promises re-encoding is a no-op"
                );
            } else {
                // The half the comment above claims and the test used to drop
                // on the floor. Three of these four inputs take this branch,
                // so without it the loop asserted one case and silently
                // skipped the rest. A caller holding a Lossy body keeps its
                // original bytes; the render it got back is a view, and
                // producing it must not have consumed or rewritten anything.
                assert_eq!(
                    body.0, bytes,
                    "a Lossy decode must leave the caller's bytes untouched"
                );
                assert_ne!(
                    encode(&doc.blocks).0,
                    bytes,
                    "a Lossy render that re-encoded to the input would have \
                     been Exact — saving it is what the interlock forbids"
                );
            }
        }
    }
}

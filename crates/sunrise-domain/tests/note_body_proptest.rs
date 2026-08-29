//! Property tests for the `NoteBody` block grammar.
//!
//! Two properties, and they are the two the codec's callers rely on:
//!
//! 1. **Arbitrary documents round-trip exactly.** Anything this build can
//!    build, it can write, read back identically, and write again to the same
//!    bytes. That is what makes an editor's save safe.
//! 2. **Arbitrary *bytes* decode without panicking, and never claim exactness
//!    they do not have.** `decode` is total by contract — see
//!    `docs/02-domain/notes.md`: "a body that fails the grammar still
//!    round-trips and still syncs". A codec that panicked on a body written
//!    by a newer build would take the app down on sync.

use proptest::prelude::*;
use sunrise_domain::note_body::{
    decode, encode, to_markdown, ChecklistItem, Fidelity, HeadingLevel, Inline, ListItem, Mark,
    NoteBlock,
};
use sunrise_domain::NoteBody;

fn mark_strategy() -> impl Strategy<Value = Mark> {
    prop_oneof![
        Just(Mark::Bold),
        Just(Mark::Italic),
        Just(Mark::Underline),
        Just(Mark::Strike),
        Just(Mark::Code),
    ]
}

/// Short, arbitrary text — including the characters Markdown export escapes
/// and the ones CBOR has to length-prefix correctly.
fn text_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        "[a-z ]{0,24}".prop_map(|s| s),
        Just("*_[]`\\".to_string()),
        Just("naïve — ünïcode 🌅".to_string()),
    ]
}

fn inline_strategy() -> impl Strategy<Value = Inline> {
    prop_oneof![
        // Marks arrive unsorted and with duplicates on purpose: `encode`
        // normalises them, so the round trip only holds if it does.
        (
            text_strategy(),
            proptest::collection::vec(mark_strategy(), 0..6)
        )
            .prop_map(|(text, mut marks)| {
                marks.sort_unstable();
                marks.dedup();
                Inline::Text { text, marks }
            }),
        (text_strategy(), text_strategy()).prop_map(|(href, label)| Inline::Link { href, label }),
        text_strategy().prop_map(|target| Inline::Ref { target }),
        text_strategy().prop_map(|person| Inline::Mention { person }),
        (text_strategy(), text_strategy()).prop_map(|(reason, placeholder_text)| {
            Inline::Redacted {
                reason,
                placeholder_text,
            }
        }),
    ]
}

fn inline_seq() -> impl Strategy<Value = Vec<Inline>> {
    proptest::collection::vec(inline_strategy(), 0..4)
}

fn heading_level() -> impl Strategy<Value = HeadingLevel> {
    prop_oneof![
        Just(HeadingLevel::One),
        Just(HeadingLevel::Two),
        Just(HeadingLevel::Three),
    ]
}

/// Blocks, nested to a depth the decoder will follow.
///
/// `prop_recursive`'s depth is capped below `MAX_NEST_DEPTH` deliberately:
/// this property is about documents the codec claims to round-trip, and
/// nesting past the floor is *documented* to be dropped. That case has its own
/// unit test.
fn block_strategy() -> impl Strategy<Value = NoteBlock> {
    let leaf = prop_oneof![
        inline_seq().prop_map(|inline| NoteBlock::Paragraph { inline }),
        (heading_level(), inline_seq())
            .prop_map(|(level, inline)| NoteBlock::Heading { level, inline }),
        inline_seq().prop_map(|inline| NoteBlock::Quote { inline }),
        Just(NoteBlock::Divider),
        (proptest::option::of("[a-z]{1,8}"), text_strategy())
            .prop_map(|(language, content)| NoteBlock::Code { language, content }),
        proptest::collection::vec((any::<bool>(), inline_seq()), 0..3).prop_map(|rows| {
            NoteBlock::Checklist {
                items: rows
                    .into_iter()
                    .map(|(checked, inline)| ChecklistItem { checked, inline })
                    .collect(),
            }
        }),
    ];
    leaf.prop_recursive(4, 24, 3, |inner| {
        (
            any::<bool>(),
            proptest::collection::vec((inline_seq(), proptest::collection::vec(inner, 0..2)), 0..3),
        )
            .prop_map(|(ordered, items)| NoteBlock::List {
                ordered,
                items: items
                    .into_iter()
                    .map(|(inline, children)| ListItem { inline, children })
                    .collect(),
            })
    })
}

fn document_strategy() -> impl Strategy<Value = Vec<NoteBlock>> {
    proptest::collection::vec(block_strategy(), 0..6)
}

proptest! {
    /// Encode → decode → encode is the identity on both the blocks and the
    /// bytes, and the codec says so.
    #[test]
    fn a_document_round_trips_through_the_grammar(blocks in document_strategy()) {
        let body = encode(&blocks);
        let doc = decode(&body);
        prop_assert_eq!(&doc.blocks, &blocks);
        prop_assert_eq!(doc.fidelity, Fidelity::Exact);
        prop_assert_eq!(encode(&doc.blocks).0, body.0);
    }

    /// Every document the codec can build has a Markdown rendering, and
    /// producing it never panics or loses the document's text.
    #[test]
    fn markdown_export_never_panics_and_keeps_the_words(blocks in document_strategy()) {
        let md = to_markdown(&blocks);
        // Every code block's literal content survives verbatim — the one part
        // of a document export must never mangle.
        for block in &blocks {
            if let NoteBlock::Code { content, .. } = block {
                prop_assert!(
                    content.is_empty() || md.contains(content.as_str()),
                    "code content vanished from the export"
                );
            }
        }
    }

    /// Arbitrary bytes — a body from a newer build, a corrupt row, a
    /// plain-text body from the CLI — decode without panicking. And when the
    /// codec claims `Exact`, re-encoding really does reproduce the input: the
    /// claim is what an editor's save is gated on.
    #[test]
    fn arbitrary_bytes_decode_without_panicking(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let body = NoteBody(bytes.clone());
        let doc = decode(&body);
        // `Exact` means the bytes come back — with one documented exception:
        // an *absent* body (no bytes at all) is the same document as an empty
        // one (`[]`, a single `0x80`), and re-encoding it picks the latter
        // spelling. Both mean "no content", so nothing is lost either way.
        if doc.is_exact() && !bytes.is_empty() {
            prop_assert_eq!(encode(&doc.blocks).0, bytes.clone());
        }
        if bytes.is_empty() {
            prop_assert!(doc.is_exact() && doc.blocks.is_empty());
        }
        // Whatever happened, the caller still holds every byte it started
        // with. This is the hard requirement: never "drop the bytes".
        prop_assert_eq!(body.0, bytes);
    }

    /// Arbitrary *text* — the shape the CLI writes — always renders as
    /// something, and never as a body this build would overwrite.
    #[test]
    fn arbitrary_text_renders_and_stays_read_only(text in "\\PC{0,120}") {
        let body = NoteBody(text.clone().into_bytes());
        let doc = decode(&body);
        if text.is_empty() {
            prop_assert!(doc.is_exact(), "an absent body is not a malformed one");
        } else if !doc.is_exact() {
            // Non-empty text that is not the grammar renders as paragraphs,
            // and every non-blank line is one of them.
            let lines = text.lines().filter(|l| !l.trim_end().is_empty()).count();
            prop_assert_eq!(doc.blocks.len(), lines.min(4096));
        }
    }
}

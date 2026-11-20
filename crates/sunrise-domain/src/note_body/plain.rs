//! The plain-text projection of a note: what search indexes.

use super::grammar::{Inline, ListItem, NoteBlock};
use super::NoteDoc;

fn inline(out: &mut String, runs: &[Inline]) {
    for r in runs {
        out.push_str(r.plain_text());
    }
}

fn items(out: &mut String, list: &[ListItem]) {
    for item in list {
        line(out);
        inline(out, &item.inline);
        blocks(out, &item.children);
    }
}

/// Start a new line unless the text is empty or already at one.
fn line(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn blocks(out: &mut String, doc: &[NoteBlock]) {
    for b in doc {
        match b {
            NoteBlock::Paragraph { inline: runs }
            | NoteBlock::Heading { inline: runs, .. }
            | NoteBlock::Quote { inline: runs } => {
                line(out);
                inline(out, runs);
            }
            NoteBlock::List { items: list, .. } => items(out, list),
            NoteBlock::Checklist { items: list } => {
                for item in list {
                    line(out);
                    inline(out, &item.inline);
                }
            }
            NoteBlock::Code { content, .. } => {
                line(out);
                out.push_str(content);
            }
            NoteBlock::Divider => {}
        }
    }
}

/// The text of every block, one per line, with no markup: the words a
/// reader sees. A body that is not the block grammar reads as its lines
/// ([`super::decode()`]), so a plain-text body is its own projection.
#[must_use]
pub fn to_plain_text(doc: &NoteDoc) -> String {
    let mut out = String::new();
    blocks(&mut out, &doc.blocks);
    out
}

/// Plain text as note blocks: one paragraph per blank-line-separated run.
///
/// The way back from typed or pasted text to a body, for every surface that
/// takes one. `\r\n` reads as `\n`; a line holding only whitespace is blank;
/// each run keeps its single line breaks, loses the whitespace around it, and
/// becomes one [`NoteBlock::Paragraph`] of plain text. Text with no words is
/// no blocks. [`to_plain_text`] of the result is the runs, one per line.
#[must_use]
pub fn from_plain_text(text: &str) -> Vec<NoteBlock> {
    let text = text.replace("\r\n", "\n");
    let mut out = Vec::new();
    let mut run: Vec<&str> = Vec::new();
    for l in text.split('\n').chain(std::iter::once("")) {
        if !l.trim().is_empty() {
            run.push(l);
        } else if !run.is_empty() {
            out.push(NoteBlock::Paragraph {
                inline: vec![Inline::text(run.join("\n").trim())],
            });
            run.clear();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::NoteBody;
    use crate::note_body::{decode, encode, ChecklistItem, HeadingLevel};

    #[test]
    fn every_block_contributes_its_words_on_its_own_line() {
        let doc = vec![
            NoteBlock::Heading {
                level: HeadingLevel::One,
                inline: vec![Inline::text("Trip")],
            },
            NoteBlock::Paragraph {
                inline: vec![Inline::text("Book "), Inline::text("東京出張")],
            },
            NoteBlock::Checklist {
                items: vec![ChecklistItem {
                    checked: false,
                    inline: vec![Inline::text("passport")],
                }],
            },
            NoteBlock::Divider,
            NoteBlock::Code {
                language: None,
                content: "x = 1".into(),
            },
        ];
        let text = to_plain_text(&decode(&encode(&doc)));
        assert_eq!(text, "Trip\nBook 東京出張\npassport\nx = 1");
    }

    #[test]
    fn a_plain_text_body_is_its_lines() {
        let body = NoteBody(b"first\n\nsecond".to_vec());
        assert_eq!(to_plain_text(&decode(&body)), "first\nsecond");
    }

    fn para(text: &str) -> NoteBlock {
        NoteBlock::Paragraph {
            inline: vec![Inline::text(text)],
        }
    }

    #[test]
    fn plain_text_is_one_paragraph_per_blank_line_run() {
        assert_eq!(
            from_plain_text("  first\nstill first  \n\n\n second \n"),
            vec![para("first\nstill first"), para("second")]
        );
    }

    #[test]
    fn crlf_reads_as_lf_and_a_whitespace_line_is_blank() {
        assert_eq!(
            from_plain_text("a\r\nb\r\n\r\nc\n \t \nd"),
            vec![para("a\nb"), para("c"), para("d")]
        );
    }

    #[test]
    fn text_with_no_words_is_no_blocks() {
        assert_eq!(from_plain_text(""), Vec::<NoteBlock>::new());
        assert_eq!(from_plain_text(" \r\n\n\t"), Vec::<NoteBlock>::new());
    }

    #[test]
    fn plain_text_round_trips_through_a_body_to_its_runs() {
        let blocks = from_plain_text("Book flights\n\n東京出張\nwith Sam\r\n\r\n");
        let doc = decode(&encode(&blocks));
        assert_eq!(doc.blocks, blocks);
        assert_eq!(to_plain_text(&doc), "Book flights\n東京出張\nwith Sam");
    }

    proptest::proptest! {
        /// Paragraphs joined by blank lines come back as those paragraphs,
        /// and their plain text is them one per line.
        #[test]
        fn paragraphs_round_trip(paras in proptest::collection::vec("[a-z]([a-z \n]{0,12}[a-z])?", 0..5)) {
            let paras: Vec<String> = paras
                .into_iter()
                .filter(|p| !p.contains("\n\n") && !p.split('\n').any(|l| l.trim().is_empty()))
                .collect();
            let blocks = from_plain_text(&paras.join("\n\n"));
            let want: Vec<NoteBlock> = paras.iter().map(|p| para(p)).collect();
            proptest::prop_assert_eq!(&blocks, &want);
            proptest::prop_assert_eq!(to_plain_text(&decode(&encode(&blocks))), paras.join("\n"));
        }
    }
}

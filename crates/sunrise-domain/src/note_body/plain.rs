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
}

//! The one-way renderer: blocks to Markdown.
//!
//! One-way deliberately. `docs/02-domain/notes.md` says "We export to
//! Markdown; we don't store as Markdown", and there is no Markdown *import*
//! here or anywhere else, because accepting Markdown back would make the
//! stored grammar the loser of every round trip. So this module reads the
//! grammar and nothing reads this module: it touches no CBOR, and the two
//! places Markdown cannot say what the grammar says are settled here alone.

use super::grammar::{Inline, Mark, NoteBlock};

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
    use super::super::grammar::{ChecklistItem, HeadingLevel, ListItem};
    use super::*;

    fn marked_text(text: &str, marks: &[Mark]) -> Inline {
        Inline::Text {
            text: text.to_string(),
            marks: marks.to_vec(),
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
}

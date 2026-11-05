//! Ported from the Apple app's `NoteEditorModelTests`: the round trips, the
//! interlock and the nesting rules, without the attributed text.

use super::*;
use sunrise_domain::note_body::Mark;

fn plain(s: &str) -> Vec<Inline> {
    vec![Inline::text(s)]
}

fn item(s: &str, children: Vec<NoteBlock>) -> ListItem {
    ListItem {
        inline: plain(s),
        children,
    }
}

fn bulleted(items: Vec<ListItem>) -> NoteBlock {
    NoteBlock::List {
        ordered: false,
        items,
    }
}

#[test]
fn every_block_kind_survives_being_loaded_and_saved_again() {
    let original = vec![
        NoteBlock::Heading {
            level: HeadingLevel::One,
            inline: plain("Trip"),
        },
        NoteBlock::Heading {
            level: HeadingLevel::Two,
            inline: plain("Packing"),
        },
        NoteBlock::Heading {
            level: HeadingLevel::Three,
            inline: plain("Carry-on"),
        },
        NoteBlock::Paragraph {
            inline: plain("Leave on Tuesday."),
        },
        bulleted(vec![item("socks", vec![])]),
        NoteBlock::List {
            ordered: true,
            items: vec![item("first", vec![])],
        },
        NoteBlock::Checklist {
            items: vec![ChecklistItem {
                checked: true,
                inline: plain("passport"),
            }],
        },
        NoteBlock::Code {
            language: Some("sh".into()),
            content: "echo hi".into(),
        },
        NoteBlock::Code {
            language: None,
            content: "plain".into(),
        },
        NoteBlock::Quote {
            inline: plain("Pack light."),
        },
        NoteBlock::Divider,
    ];
    assert_eq!(edit_lock(&original, true), None);
    assert_eq!(note_blocks(&editor_blocks(&original)), original);
}

#[test]
fn every_mark_survives() {
    let marked = vec![NoteBlock::Paragraph {
        inline: vec![
            Inline::Text {
                text: "bold".into(),
                marks: vec![Mark::Bold],
            },
            Inline::Text {
                text: "all".into(),
                marks: vec![
                    Mark::Bold,
                    Mark::Italic,
                    Mark::Underline,
                    Mark::Strike,
                    Mark::Code,
                ],
            },
        ],
    }];
    assert_eq!(edit_lock(&marked, true), None);
    assert_eq!(note_blocks(&editor_blocks(&marked)), marked);
}

#[test]
fn a_nested_list_keeps_its_shape_as_indentation() {
    let nested = vec![bulleted(vec![item(
        "outer",
        vec![bulleted(vec![item("inner", vec![])])],
    )])];
    let blocks = editor_blocks(&nested);
    let depths: Vec<_> = blocks[0].rows.iter().map(|r| r.depth).collect();
    assert_eq!(depths, [0, 1]);
    assert_eq!(edit_lock(&nested, true), None);
    assert_eq!(note_blocks(&blocks), nested);
}

#[test]
fn a_body_the_core_could_not_re_encode_is_unreadable() {
    assert_eq!(edit_lock(&[], false), Some(EditLock::Unreadable));
}

#[test]
fn a_reference_or_a_block_under_a_list_item_is_unsupported() {
    let reference = vec![NoteBlock::Paragraph {
        inline: vec![Inline::Ref {
            target: "tsk_01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        }],
    }];
    assert_eq!(edit_lock(&reference, true), Some(EditLock::Unsupported));
    let under = vec![bulleted(vec![item(
        "outer",
        vec![NoteBlock::Code {
            language: None,
            content: "echo hi".into(),
        }],
    )])];
    assert_eq!(edit_lock(&under, true), Some(EditLock::Unsupported));
    let mixed = vec![bulleted(vec![item(
        "outer",
        vec![NoteBlock::List {
            ordered: true,
            items: vec![item("inner", vec![])],
        }],
    )])];
    assert_eq!(
        edit_lock(&mixed, true),
        Some(EditLock::Unsupported),
        "a nested list takes its parent's order"
    );
}

#[test]
fn emptiness_is_per_kind_and_a_divider_is_never_empty() {
    assert!(EditorBlock::default().is_empty());
    assert!(!EditorBlock::empty(NoteBlockKind::Divider).is_empty());
    let mut list = EditorBlock::empty(NoteBlockKind::Bulleted);
    list.rows.push(EditorRow::default());
    assert!(list.is_empty());
    list.rows[0].inline = plain("x");
    assert!(!list.is_empty());
    let mut code = EditorBlock::empty(NoteBlockKind::Code);
    code.code = "x".into();
    assert!(!code.is_empty());
}

#[test]
fn changing_kinds_keeps_the_words() {
    let mut b = EditorBlock::from_note_block(&NoteBlock::Paragraph {
        inline: plain("Words"),
    });
    set_kind(&mut b, NoteBlockKind::Heading2);
    assert_eq!(
        b.to_note_block(),
        NoteBlock::Heading {
            level: HeadingLevel::Two,
            inline: plain("Words")
        }
    );
    let mut list = EditorBlock::from_note_block(&bulleted(vec![
        item("one", vec![bulleted(vec![item("two", vec![])])]),
        item("", vec![]),
        item("three", vec![]),
    ]));
    let mut flat = list.clone();
    set_kind(&mut flat, NoteBlockKind::Paragraph);
    assert_eq!(
        flat.to_note_block(),
        NoteBlock::Paragraph {
            inline: vec![
                Inline::text("one"),
                Inline::text(" "),
                Inline::text("two"),
                Inline::text(" "),
                Inline::text("three")
            ]
        },
        "rows are joined, not dropped"
    );
    set_kind(&mut list, NoteBlockKind::Checklist);
    assert!(
        list.rows.iter().all(|r| r.depth == 0),
        "a checklist cannot nest"
    );
    let mut para = EditorBlock::from_note_block(&NoteBlock::Paragraph {
        inline: plain("item"),
    });
    set_kind(&mut para, NoteBlockKind::Numbered);
    assert_eq!(para.rows.len(), 1);
    assert!(para.text.is_empty());
}

#[test]
fn the_first_row_cannot_indent_and_no_row_more_than_one_below_the_row_above() {
    let mut b =
        EditorBlock::from_note_block(&bulleted(vec![item("one", vec![]), item("two", vec![])]));
    assert!(!indent_row(&mut b, 0), "the first row has no parent");
    assert!(indent_row(&mut b, 1));
    assert!(!indent_row(&mut b, 1), "refused a second time");
    assert_eq!(b.rows[1].depth, 1);
    let mut check = EditorBlock::from_note_block(&NoteBlock::Checklist {
        items: vec![ChecklistItem::default(), ChecklistItem::default()],
    });
    assert!(!indent_row(&mut check, 1), "a checklist is flat");
}

#[test]
fn outdenting_a_row_pulls_its_orphaned_children_back_with_it() {
    let mut b = EditorBlock::from_note_block(&bulleted(vec![item(
        "one",
        vec![bulleted(vec![item(
            "two",
            vec![bulleted(vec![item("three", vec![])])],
        )])],
    )]));
    let depths = |b: &EditorBlock| b.rows.iter().map(|r| r.depth).collect::<Vec<_>>();
    assert_eq!(depths(&b), [0, 1, 2]);
    assert!(outdent_row(&mut b, 1));
    assert_eq!(depths(&b), [0, 0, 1]);
    assert!(!outdent_row(&mut b, 0), "already at the top");
    assert!(!outdent_row(&mut b, 9));
}

#[test]
fn indented_rows_round_trip_through_the_grammar() {
    let mut b = EditorBlock::empty(NoteBlockKind::Bulleted);
    b.rows = vec![
        EditorRow {
            inline: plain("outer"),
            ..EditorRow::default()
        },
        EditorRow {
            inline: plain("inner"),
            ..EditorRow::default()
        },
    ];
    assert!(indent_row(&mut b, 1));
    assert_eq!(
        b.to_note_block(),
        bulleted(vec![item(
            "outer",
            vec![bulleted(vec![item("inner", vec![])])]
        )])
    );
}

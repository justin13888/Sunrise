//! The note editor's block model, lowered onto UniFFI:
//! [`sunrise_client_core::note_blocks`]. Free functions over records, so a
//! client keeps its blocks in whatever observable shape it draws and hands
//! them here to convert, to check and to restructure.
//!
//! `CoreNoteBlockKind` carries a `Core` prefix because the Apple app already
//! declares a `NoteBlockKind`, which this replaces in a later pass.

use sunrise_client_core::note_blocks::{self, EditLock, EditorBlock, EditorRow, NoteBlockKind};
use sunrise_domain::note_body;

use crate::notes::{NoteBlock, NoteInline};

/// See [`sunrise_client_core::note_blocks::NoteBlockKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CoreNoteBlockKind {
    /// A paragraph.
    Paragraph,
    /// `#`.
    Heading1,
    /// `##`.
    Heading2,
    /// `###`.
    Heading3,
    /// An unordered list.
    Bulleted,
    /// An ordered list.
    Numbered,
    /// A checklist.
    Checklist,
    /// Code.
    Code,
    /// A quote.
    Quote,
    /// A divider.
    Divider,
}

impl From<NoteBlockKind> for CoreNoteBlockKind {
    fn from(k: NoteBlockKind) -> Self {
        match k {
            NoteBlockKind::Paragraph => Self::Paragraph,
            NoteBlockKind::Heading1 => Self::Heading1,
            NoteBlockKind::Heading2 => Self::Heading2,
            NoteBlockKind::Heading3 => Self::Heading3,
            NoteBlockKind::Bulleted => Self::Bulleted,
            NoteBlockKind::Numbered => Self::Numbered,
            NoteBlockKind::Checklist => Self::Checklist,
            NoteBlockKind::Code => Self::Code,
            NoteBlockKind::Quote => Self::Quote,
            NoteBlockKind::Divider => Self::Divider,
        }
    }
}

impl From<CoreNoteBlockKind> for NoteBlockKind {
    fn from(k: CoreNoteBlockKind) -> Self {
        match k {
            CoreNoteBlockKind::Paragraph => Self::Paragraph,
            CoreNoteBlockKind::Heading1 => Self::Heading1,
            CoreNoteBlockKind::Heading2 => Self::Heading2,
            CoreNoteBlockKind::Heading3 => Self::Heading3,
            CoreNoteBlockKind::Bulleted => Self::Bulleted,
            CoreNoteBlockKind::Numbered => Self::Numbered,
            CoreNoteBlockKind::Checklist => Self::Checklist,
            CoreNoteBlockKind::Code => Self::Code,
            CoreNoteBlockKind::Quote => Self::Quote,
            CoreNoteBlockKind::Divider => Self::Divider,
        }
    }
}

/// What a kind holds, for a picker and a form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct NoteBlockKindTraits {
    /// Rows rather than one run of text.
    pub has_rows: bool,
    /// Rows may indent.
    pub allows_nesting: bool,
    /// One editable run of inline text.
    pub has_text: bool,
}

/// What `kind` holds.
#[uniffi::export]
#[must_use]
pub fn note_block_kind_traits(kind: CoreNoteBlockKind) -> NoteBlockKindTraits {
    let kind = NoteBlockKind::from(kind);
    NoteBlockKindTraits {
        has_rows: kind.has_rows(),
        allows_nesting: kind.allows_nesting(),
        has_text: kind.has_text(),
    }
}

/// See [`sunrise_client_core::note_blocks::EditorRow`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NoteEditorRowIn {
    /// The row's text.
    pub inline: Vec<NoteInline>,
    /// Ticked, for a checklist row.
    pub checked: bool,
    /// Nesting, zero-based.
    pub depth: u32,
}

/// See [`sunrise_client_core::note_blocks::EditorBlock`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct NoteEditorBlockIn {
    /// What it is.
    pub kind: CoreNoteBlockKind,
    /// Paragraph, heading and quote content.
    pub text: Vec<NoteInline>,
    /// List and checklist rows.
    pub rows: Vec<NoteEditorRowIn>,
    /// Code content.
    pub code: String,
    /// Code language; empty for none.
    pub language: String,
}

fn runs_in(runs: Vec<NoteInline>) -> Vec<note_body::Inline> {
    runs.into_iter().map(note_body::Inline::from).collect()
}

fn runs_out(runs: Vec<note_body::Inline>) -> Vec<NoteInline> {
    runs.into_iter().map(NoteInline::from).collect()
}

impl From<NoteEditorBlockIn> for EditorBlock {
    fn from(b: NoteEditorBlockIn) -> Self {
        Self {
            kind: b.kind.into(),
            text: runs_in(b.text),
            rows: b
                .rows
                .into_iter()
                .map(|r| EditorRow {
                    inline: runs_in(r.inline),
                    checked: r.checked,
                    depth: r.depth,
                })
                .collect(),
            code: b.code,
            language: b.language,
        }
    }
}

impl From<EditorBlock> for NoteEditorBlockIn {
    fn from(b: EditorBlock) -> Self {
        Self {
            kind: b.kind.into(),
            text: runs_out(b.text),
            rows: b
                .rows
                .into_iter()
                .map(|r| NoteEditorRowIn {
                    inline: runs_out(r.inline),
                    checked: r.checked,
                    depth: r.depth,
                })
                .collect(),
            code: b.code,
            language: b.language,
        }
    }
}

fn stored(blocks: Vec<NoteBlock>) -> Vec<note_body::NoteBlock> {
    blocks.into_iter().map(note_body::NoteBlock::from).collect()
}

/// The editable form of decoded blocks.
#[uniffi::export]
#[must_use]
pub fn note_editor_blocks(blocks: Vec<NoteBlock>) -> Vec<NoteEditorBlockIn> {
    note_blocks::editor_blocks(&stored(blocks))
        .into_iter()
        .map(NoteEditorBlockIn::from)
        .collect()
}

/// The stored form of edited blocks, for `encode_note_body`.
#[uniffi::export]
#[must_use]
pub fn note_editor_save(blocks: Vec<NoteEditorBlockIn>) -> Vec<NoteBlock> {
    let editable: Vec<EditorBlock> = blocks.into_iter().map(EditorBlock::from).collect();
    note_blocks::note_blocks(&editable)
        .into_iter()
        .map(NoteBlock::from)
        .collect()
}

/// See [`sunrise_client_core::note_blocks::EditLock`].
#[uniffi::remote(Enum)]
pub enum EditLock {
    Unreadable,
    Unsupported,
}

/// Why a decoded body opens read-only, or `None` when it may be edited.
/// `exact` is the decode's fidelity.
#[uniffi::export]
#[must_use]
pub fn note_edit_lock(blocks: Vec<NoteBlock>, exact: bool) -> Option<EditLock> {
    note_blocks::edit_lock(&stored(blocks), exact)
}

/// `block` as `kind`, keeping its words.
#[uniffi::export]
#[must_use]
pub fn note_block_set_kind(block: NoteEditorBlockIn, kind: CoreNoteBlockKind) -> NoteEditorBlockIn {
    let mut block = EditorBlock::from(block);
    note_blocks::set_kind(&mut block, kind.into());
    block.into()
}

/// `block` with row `index` indented, or unchanged where it cannot be.
#[uniffi::export]
#[must_use]
pub fn note_block_indent_row(block: NoteEditorBlockIn, index: u32) -> NoteEditorBlockIn {
    let mut block = EditorBlock::from(block);
    note_blocks::indent_row(&mut block, index as usize);
    block.into()
}

/// `block` with row `index` outdented and the rows below re-parented, or
/// unchanged where it cannot be.
#[uniffi::export]
#[must_use]
pub fn note_block_outdent_row(block: NoteEditorBlockIn, index: u32) -> NoteEditorBlockIn {
    let mut block = EditorBlock::from(block);
    note_blocks::outdent_row(&mut block, index as usize);
    block.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exports_round_trip_and_restructure() {
        let list = NoteBlock::List {
            ordered: false,
            items: vec![
                crate::notes::NoteListItem {
                    inline: vec![NoteInline::Text {
                        text: "one".into(),
                        marks: vec![],
                    }],
                    children: vec![],
                },
                crate::notes::NoteListItem {
                    inline: vec![],
                    children: vec![],
                },
            ],
        };
        assert_eq!(note_edit_lock(vec![list.clone()], true), None);
        let editable = note_editor_blocks(vec![list.clone()]);
        assert_eq!(note_editor_save(editable.clone()), vec![list]);
        let indented = note_block_indent_row(editable[0].clone(), 1);
        assert_eq!(indented.rows[1].depth, 1);
        assert_eq!(note_block_outdent_row(indented, 1).rows[1].depth, 0);
        let para = note_block_set_kind(editable[0].clone(), CoreNoteBlockKind::Paragraph);
        assert!(para.rows.is_empty());
        assert!(note_block_kind_traits(CoreNoteBlockKind::Bulleted).allows_nesting);
        assert_eq!(note_edit_lock(vec![], false), Some(EditLock::Unreadable));
    }
}

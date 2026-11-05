//! A note body in the shape a block editor edits, and back.
//!
//! The stored grammar nests lists (`ListItem::children`) and spells headings
//! by level; an editor offers a flat column of blocks, a kind picker with
//! three heading entries, and indent and outdent on list rows. This module is
//! the conversion between the two and the check that makes an editor's
//! limits safe rather than lossy: a body whose editable form does not convert
//! back to the same blocks is opened read-only, never flattened and saved.
//! Ported from the Apple app's `NoteEditorBlock.swift` and the interlock in
//! `NoteEditorModel.load`; the text runs stay `Inline`s here, and turning them
//! into a platform's attributed text is the client's.

use sunrise_domain::note_body::{ChecklistItem, HeadingLevel, Inline, ListItem, NoteBlock};

/// What a block is, as a picker offers it: flatter than [`NoteBlock`], with a
/// heading per level and ordered and unordered lists as two kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoteBlockKind {
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
    /// A code block.
    Code,
    /// A quote.
    Quote,
    /// A divider.
    Divider,
}

impl NoteBlockKind {
    /// Every kind, in picker order.
    pub const ALL: [Self; 10] = [
        Self::Paragraph,
        Self::Heading1,
        Self::Heading2,
        Self::Heading3,
        Self::Bulleted,
        Self::Numbered,
        Self::Checklist,
        Self::Code,
        Self::Quote,
        Self::Divider,
    ];

    /// Whether the kind holds rows rather than one run of text.
    #[must_use]
    pub const fn has_rows(self) -> bool {
        matches!(self, Self::Bulleted | Self::Numbered | Self::Checklist)
    }

    /// Whether rows may indent under one another. A checklist is flat: the
    /// grammar's checklist item has no children.
    #[must_use]
    pub const fn allows_nesting(self) -> bool {
        matches!(self, Self::Bulleted | Self::Numbered)
    }

    /// Whether the kind carries one editable run of inline text.
    #[must_use]
    pub const fn has_text(self) -> bool {
        matches!(
            self,
            Self::Paragraph | Self::Heading1 | Self::Heading2 | Self::Heading3 | Self::Quote
        )
    }

    const fn heading_level(self) -> Option<HeadingLevel> {
        match self {
            Self::Heading1 => Some(HeadingLevel::One),
            Self::Heading2 => Some(HeadingLevel::Two),
            Self::Heading3 => Some(HeadingLevel::Three),
            _ => None,
        }
    }

    const fn heading(level: HeadingLevel) -> Self {
        match level {
            HeadingLevel::One => Self::Heading1,
            HeadingLevel::Two => Self::Heading2,
            HeadingLevel::Three => Self::Heading3,
        }
    }
}

/// One row of a list or checklist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditorRow {
    /// The row's text.
    pub inline: Vec<Inline>,
    /// Ticked, for a checklist row.
    pub checked: bool,
    /// Nesting, zero-based; only bulleted and numbered lists use it.
    pub depth: u32,
}

/// One block, in the shape a form binds to: a kind and every field a kind
/// might need. The fields a kind does not use are not read —
/// [`Self::to_note_block`] alone decides what a block means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorBlock {
    /// What it is.
    pub kind: NoteBlockKind,
    /// Paragraph, heading and quote content.
    pub text: Vec<Inline>,
    /// List and checklist content.
    pub rows: Vec<EditorRow>,
    /// Code content, literal.
    pub code: String,
    /// The code's language hint; empty for none.
    pub language: String,
}

impl Default for EditorBlock {
    fn default() -> Self {
        Self::empty(NoteBlockKind::Paragraph)
    }
}

/// Whether a run of inline content has no characters.
fn inline_is_empty(inline: &[Inline]) -> bool {
    inline.iter().all(|i| i.plain_text().is_empty())
}

impl EditorBlock {
    /// An empty block of `kind`.
    #[must_use]
    pub const fn empty(kind: NoteBlockKind) -> Self {
        Self {
            kind,
            text: Vec::new(),
            rows: Vec::new(),
            code: String::new(),
            language: String::new(),
        }
    }

    /// Whether the block would contribute nothing to a rendered note. A
    /// divider is never empty: it is content with no text.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self.kind {
            NoteBlockKind::Divider => false,
            NoteBlockKind::Code => self.code.is_empty(),
            NoteBlockKind::Bulleted | NoteBlockKind::Numbered | NoteBlockKind::Checklist => {
                self.rows.iter().all(|r| inline_is_empty(&r.inline))
            }
            _ => inline_is_empty(&self.text),
        }
    }

    /// The editable form of a stored block.
    ///
    /// Nested list items become rows carrying a depth. A child that is not a
    /// list has nowhere to go and is dropped — which [`edit_lock`] catches,
    /// so the note opens read-only rather than losing it on save.
    #[must_use]
    pub fn from_note_block(block: &NoteBlock) -> Self {
        match block {
            NoteBlock::Paragraph { inline } => Self::with_text(NoteBlockKind::Paragraph, inline),
            NoteBlock::Heading { level, inline } => {
                Self::with_text(NoteBlockKind::heading(*level), inline)
            }
            NoteBlock::Quote { inline } => Self::with_text(NoteBlockKind::Quote, inline),
            NoteBlock::List { ordered, items } => {
                let kind = if *ordered {
                    NoteBlockKind::Numbered
                } else {
                    NoteBlockKind::Bulleted
                };
                let mut b = Self::empty(kind);
                flatten(items, 0, &mut b.rows);
                b
            }
            NoteBlock::Checklist { items } => {
                let mut b = Self::empty(NoteBlockKind::Checklist);
                b.rows = items
                    .iter()
                    .map(|i| EditorRow {
                        inline: i.inline.clone(),
                        checked: i.checked,
                        depth: 0,
                    })
                    .collect();
                b
            }
            NoteBlock::Code { language, content } => {
                let mut b = Self::empty(NoteBlockKind::Code);
                b.code.clone_from(content);
                b.language = language.clone().unwrap_or_default();
                b
            }
            NoteBlock::Divider => Self::empty(NoteBlockKind::Divider),
        }
    }

    fn with_text(kind: NoteBlockKind, inline: &[Inline]) -> Self {
        let mut b = Self::empty(kind);
        b.text = inline.to_vec();
        b
    }

    /// What this block means, in the vocabulary the core stores. A nested
    /// list inherits its parent's ordering: indent and outdent cannot say
    /// otherwise.
    #[must_use]
    pub fn to_note_block(&self) -> NoteBlock {
        let inline = self.text.clone();
        match self.kind {
            NoteBlockKind::Paragraph => NoteBlock::Paragraph { inline },
            NoteBlockKind::Heading1 | NoteBlockKind::Heading2 | NoteBlockKind::Heading3 => {
                NoteBlock::Heading {
                    level: self.kind.heading_level().unwrap_or(HeadingLevel::One),
                    inline,
                }
            }
            NoteBlockKind::Quote => NoteBlock::Quote { inline },
            NoteBlockKind::Bulleted | NoteBlockKind::Numbered => {
                let ordered = self.kind == NoteBlockKind::Numbered;
                let mut cursor = 0;
                NoteBlock::List {
                    ordered,
                    items: build(&self.rows, &mut cursor, 0, ordered),
                }
            }
            NoteBlockKind::Checklist => NoteBlock::Checklist {
                items: self
                    .rows
                    .iter()
                    .map(|r| ChecklistItem {
                        checked: r.checked,
                        inline: r.inline.clone(),
                    })
                    .collect(),
            },
            NoteBlockKind::Code => NoteBlock::Code {
                language: (!self.language.is_empty()).then(|| self.language.clone()),
                content: self.code.clone(),
            },
            NoteBlockKind::Divider => NoteBlock::Divider,
        }
    }
}

fn flatten(items: &[ListItem], depth: u32, rows: &mut Vec<EditorRow>) {
    for item in items {
        rows.push(EditorRow {
            inline: item.inline.clone(),
            checked: false,
            depth,
        });
        for child in &item.children {
            if let NoteBlock::List { items, .. } = child {
                flatten(items, depth + 1, rows);
            }
        }
    }
}

fn build(rows: &[EditorRow], cursor: &mut usize, depth: u32, ordered: bool) -> Vec<ListItem> {
    let mut items = Vec::new();
    while *cursor < rows.len() && rows[*cursor].depth >= depth {
        let row = &rows[*cursor];
        *cursor += 1;
        let mut children = Vec::new();
        if *cursor < rows.len() && rows[*cursor].depth > depth {
            let nested = build(rows, cursor, depth + 1, ordered);
            if !nested.is_empty() {
                children.push(NoteBlock::List {
                    ordered,
                    items: nested,
                });
            }
        }
        items.push(ListItem {
            inline: row.inline.clone(),
            children,
        });
    }
    items
}

/// The editable form of a whole body.
#[must_use]
pub fn editor_blocks(blocks: &[NoteBlock]) -> Vec<EditorBlock> {
    blocks.iter().map(EditorBlock::from_note_block).collect()
}

/// The stored form of an edited body.
#[must_use]
pub fn note_blocks(blocks: &[EditorBlock]) -> Vec<NoteBlock> {
    blocks.iter().map(EditorBlock::to_note_block).collect()
}

/// Why a body opens read-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EditLock {
    /// The core could not re-encode the body exactly: a newer build wrote
    /// something this one cannot put back.
    Unreadable,
    /// The editor's own form does not convert back to the same blocks: a
    /// reference, a mention or a redaction in the text, a non-list child
    /// under a list item, or a numbered list nested in a bulleted one.
    Unsupported,
}

/// Whether a decoded body may be edited: `exact` is the core's fidelity, and
/// the body must survive the round trip through [`editor_blocks`] and
/// [`note_blocks`] with only text and links in its runs, which is what an
/// attributed-text editor carries.
#[must_use]
pub fn edit_lock(blocks: &[NoteBlock], exact: bool) -> Option<EditLock> {
    if !exact {
        return Some(EditLock::Unreadable);
    }
    let editable = editor_blocks(blocks);
    let plain_runs = editable
        .iter()
        .all(|b| runs_editable(&b.text) && b.rows.iter().all(|r| runs_editable(&r.inline)));
    if !plain_runs || note_blocks(&editable) != blocks {
        return Some(EditLock::Unsupported);
    }
    None
}

fn runs_editable(inline: &[Inline]) -> bool {
    inline
        .iter()
        .all(|i| matches!(i, Inline::Text { .. } | Inline::Link { .. }))
}

/// Change what a block is. Rows become one run of text (non-empty rows
/// joined by a space) and text becomes one row; a kind that does not nest
/// flattens its rows.
pub fn set_kind(block: &mut EditorBlock, kind: NoteBlockKind) {
    let old = block.kind;
    if old == kind {
        return;
    }
    if old.has_rows() && !kind.has_rows() {
        let mut text: Vec<Inline> = Vec::new();
        for row in block.rows.drain(..) {
            if inline_is_empty(&row.inline) {
                continue;
            }
            if !text.is_empty() {
                text.push(Inline::text(" "));
            }
            text.extend(row.inline);
        }
        block.text = text;
    } else if !old.has_rows() && kind.has_rows() {
        block.rows = vec![EditorRow {
            inline: std::mem::take(&mut block.text),
            checked: false,
            depth: 0,
        }];
    }
    if kind.has_rows() && !kind.allows_nesting() {
        for row in &mut block.rows {
            row.depth = 0;
        }
    }
    block.kind = kind;
}

/// Make `rows[index]` one level deeper: only in a kind that nests, and only
/// under a row above it that is at least as deep. Whether it moved.
pub fn indent_row(block: &mut EditorBlock, index: usize) -> bool {
    if !block.kind.allows_nesting() || index == 0 || index >= block.rows.len() {
        return false;
    }
    if block.rows[index].depth > block.rows[index - 1].depth {
        return false;
    }
    block.rows[index].depth += 1;
    true
}

/// Make `rows[index]` one level shallower, then pull every row back to a
/// depth its predecessor can parent: the grammar nests by containment, so an
/// orphaned row is unwritable, not merely untidy. Whether it moved.
pub fn outdent_row(block: &mut EditorBlock, index: usize) -> bool {
    match block.rows.get_mut(index) {
        Some(row) if row.depth > 0 => row.depth -= 1,
        _ => return false,
    }
    let mut ceiling = 0;
    for row in &mut block.rows {
        row.depth = row.depth.min(ceiling);
        ceiling = row.depth + 1;
    }
    true
}

#[cfg(test)]
mod tests;

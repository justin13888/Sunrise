import Foundation
import SwiftUI

/// Why a note is being shown rather than edited.
///
/// Read-only is a first-class state here, not a failure. A note body is one
/// last-writer-wins unit (ADR-0014), so a block this build dropped on save
/// would not stay local — it would propagate to every device that synced
/// afterwards. Refusing to type is cheaper than that, every time.
enum NoteEditorLock: Equatable {
    /// The core could not put the body back byte for byte: a block kind or a
    /// mark from a newer build, or bytes that were never the grammar.
    case unreadable
    /// The core read it fine, but this editor has no controls for some of it
    /// — an entity reference, a mention, or nesting it cannot draw.
    case unsupported

    var message: String {
        switch self {
        case .unreadable:
            "This note was written by a newer version of Sunrise. "
                + "You can read it here; editing it would drop the parts this version cannot show."
        case .unsupported:
            "This note uses formatting this editor cannot change yet — a reference, a mention, "
                + "or nested content. You can read it here, and edit it in a later version."
        }
    }
}

/// Editing a task's note body.
///
/// All the editing logic lives here rather than in a view body, so it can be
/// tested without a window: every mutation below is a method on a model, and
/// ``NoteBodyEditor`` only binds to what this exposes.
///
/// # The two interlocks
///
/// 1. The core reports whether it could re-encode the body exactly
///    (`NoteFidelity`). Anything else is ``NoteEditorLock/unreadable``.
/// 2. This model then checks whether *its own* editable form round-trips:
///    it converts the decoded blocks into ``NoteEditorBlock``s, converts them
///    straight back, and compares. Anything that does not survive that is
///    ``NoteEditorLock/unsupported``.
///
/// The second check is the one that makes the editor's limits safe rather
/// than lossy. It does not enumerate what the editor cannot draw; it asks
/// whether what it drew means the same thing.
@MainActor
@Observable
final class NoteEditorModel {
    /// The blocks on screen. Populated even when locked — the whole point is
    /// that an unreadable note still renders.
    ///
    /// Settable so a text field can bind straight into a block's characters,
    /// which is the one edit SwiftUI should not have to route through a
    /// method. Every *structural* change — what a block is, how many there
    /// are, what order they are in — goes through the methods below, which is
    /// where the rules about depth and read-only live.
    var blocks: [NoteEditorBlock] = []
    /// Why editing is refused, or `nil` when it is not.
    private(set) var lock: NoteEditorLock?
    /// Which block the mark buttons act on.
    var focusedBlock: UUID?

    /// What the note was when it was opened, for the dirty check.
    private var loaded: [NoteBlock] = []

    init(body: NoteBody? = nil) {
        load(body: body)
    }

    // MARK: - Loading

    /// Read a body, deciding as it goes whether it may be edited.
    func load(body: NoteBody?) {
        let document = decodeNoteBody(body: body ?? NoteBody())
        loaded = document.blocks
        blocks = document.blocks.map(NoteEditorBlock.init(from:))

        if document.fidelity == .lossy {
            lock = .unreadable
        } else if blocks.map(\.noteBlock) != document.blocks {
            lock = .unsupported
        } else {
            lock = nil
        }

        // An empty, editable note needs somewhere to type. An empty locked
        // one does not — there is nothing to say about it.
        if blocks.isEmpty, lock == nil {
            blocks = [NoteEditorBlock()]
        }
        focusedBlock = blocks.first?.id
    }

    // MARK: - State

    var isReadOnly: Bool { lock != nil }

    /// Whether the note would render as nothing at all.
    var isEmpty: Bool { blocks.allSatisfy(\.isEmpty) }

    /// Whether anything has changed since it was opened.
    ///
    /// Compared as blocks, not as bytes: re-encoding is deterministic, so
    /// this is the same answer with less work, and it is the answer that stays
    /// right if the encoder ever learns a shorter spelling.
    var isDirty: Bool { !isReadOnly && noteBlocks != loaded }

    /// What would be written.
    var noteBlocks: [NoteBlock] { blocks.map(\.noteBlock) }

    /// The note as Markdown, worded by the core so the app and the CLI agree.
    var markdown: String { noteBodyMarkdown(blocks: noteBlocks) }

    /// How close the note is to the size the core starts to complain about.
    /// `nil` until it is worth saying anything.
    var lengthWarning: String? {
        let limits = noteBodyLimits()
        let size = UInt64(encodeNoteBody(blocks: noteBlocks).count)
        guard size >= limits.softBytes else { return nil }
        if size >= limits.maxBytes {
            return "This note is at the size limit. Split it into two."
        }
        return "This note is getting long. Consider splitting it."
    }

    // MARK: - Writing

    /// Fold the note into a task edit.
    ///
    /// Sets neither field when the note is locked or unchanged, which is what
    /// leaves the stored body alone: `TaskEdit` treats an unset `setBody` and
    /// an unset `clearBody` as "do not touch this".
    func apply(to edit: inout TaskEdit) {
        guard isDirty else { return }
        if isEmpty {
            edit.clearBody = true
        } else {
            edit.setBody = encodeNoteBody(blocks: noteBlocks)
        }
    }

    // MARK: - Block edits

    private func index(of id: UUID) -> Int? {
        blocks.firstIndex { $0.id == id }
    }

    /// Change what a block is.
    ///
    /// Content follows where it can: the text of a paragraph becomes the text
    /// of a heading, and the rows of a bulleted list become the rows of a
    /// checklist. Turning a list into a paragraph joins its rows, because
    /// throwing them away silently is the one behaviour nobody expects.
    func setKind(_ kind: NoteBlockKind, for id: UUID) {
        guard !isReadOnly, let index = index(of: id) else { return }
        let old = blocks[index].kind
        guard old != kind else { return }

        if old.hasRows, !kind.hasRows {
            blocks[index].text = joined(blocks[index].rows)
            blocks[index].rows = []
        } else if !old.hasRows, kind.hasRows {
            blocks[index].rows = [NoteEditorRow(text: blocks[index].text)]
            blocks[index].text = AttributedString()
        }
        if kind.hasRows, !kind.allowsNesting {
            for row in blocks[index].rows.indices { blocks[index].rows[row].depth = 0 }
        }
        blocks[index].kind = kind
    }

    private func joined(_ rows: [NoteEditorRow]) -> AttributedString {
        var out = AttributedString()
        for row in rows where !NoteInlineText.isEmpty(row.text) {
            if !out.characters.isEmpty { out.append(AttributedString(" ")) }
            out.append(row.text)
        }
        return out
    }

    /// Add a block below `id`, or at the end when it is `nil`.
    @discardableResult
    func insertBlock(_ kind: NoteBlockKind = .paragraph, after id: UUID?) -> UUID? {
        guard !isReadOnly else { return nil }
        var block = NoteEditorBlock(kind: kind)
        if kind.hasRows { block.rows = [NoteEditorRow()] }
        let at = id.flatMap(index(of:)).map { $0 + 1 } ?? blocks.count
        blocks.insert(block, at: at)
        focusedBlock = block.id
        return block.id
    }

    /// Remove a block. The last one is emptied instead of removed, so there
    /// is always somewhere to type.
    func removeBlock(_ id: UUID) {
        guard !isReadOnly, let index = index(of: id) else { return }
        if blocks.count == 1 {
            blocks = [NoteEditorBlock()]
            focusedBlock = blocks[0].id
            return
        }
        blocks.remove(at: index)
        focusedBlock = blocks[min(index, blocks.count - 1)].id
    }

    /// Move a block up or down by one.
    func moveBlock(_ id: UUID, by offset: Int) {
        guard !isReadOnly, let index = index(of: id) else { return }
        let target = index + offset
        guard blocks.indices.contains(target) else { return }
        blocks.swapAt(index, target)
    }

    // MARK: - Row edits

    /// Add a row below `rowID`, or at the end of the block when it is `nil`.
    func addRow(to id: UUID, after rowID: UUID? = nil) {
        guard !isReadOnly, let index = index(of: id), blocks[index].kind.hasRows else { return }
        let rows = blocks[index].rows
        let at = rowID.flatMap { row in rows.firstIndex { $0.id == row } }.map { $0 + 1 } ?? rows.count
        // A new row starts at the depth of the one above it, which is what
        // pressing Return in a nested list means.
        let depth = at > 0 ? rows[at - 1].depth : 0
        blocks[index].rows.insert(NoteEditorRow(depth: depth), at: at)
    }

    func removeRow(_ rowID: UUID, from id: UUID) {
        guard !isReadOnly, let index = index(of: id) else { return }
        guard blocks[index].rows.count > 1 else { return }
        blocks[index].rows.removeAll { $0.id == rowID }
        normalizeDepths(in: index)
    }

    func toggleChecked(_ rowID: UUID, in id: UUID) {
        guard !isReadOnly, let index = index(of: id) else { return }
        guard let row = blocks[index].rows.firstIndex(where: { $0.id == rowID }) else { return }
        blocks[index].rows[row].checked.toggle()
    }

    /// Indent a row under the one above it.
    ///
    /// Refused for the first row, and for any row that would end up more than
    /// one level below its predecessor: an orphaned indent has no parent to
    /// hang from, and the grammar has no way to write one down.
    func indentRow(_ rowID: UUID, in id: UUID) {
        guard !isReadOnly, let index = index(of: id), blocks[index].kind.allowsNesting else { return }
        guard let row = blocks[index].rows.firstIndex(where: { $0.id == rowID }), row > 0 else { return }
        guard blocks[index].rows[row].depth <= blocks[index].rows[row - 1].depth else { return }
        blocks[index].rows[row].depth += 1
    }

    func outdentRow(_ rowID: UUID, in id: UUID) {
        guard !isReadOnly, let index = index(of: id) else { return }
        guard let row = blocks[index].rows.firstIndex(where: { $0.id == rowID }) else { return }
        guard blocks[index].rows[row].depth > 0 else { return }
        blocks[index].rows[row].depth -= 1
        normalizeDepths(in: index)
    }

    /// Pull every row back to a depth its predecessor can parent.
    ///
    /// Removing or outdenting a row can orphan the ones beneath it. The
    /// grammar nests by containment, so an orphan is not merely ugly — it is
    /// unwritable, and the rebuild would quietly reparent it somewhere else.
    private func normalizeDepths(in index: Int) {
        var ceiling = 0
        for row in blocks[index].rows.indices {
            let depth = min(blocks[index].rows[row].depth, ceiling)
            blocks[index].rows[row].depth = depth
            ceiling = depth + 1
        }
    }

    // MARK: - Marks

    /// Whether the focused block's selection already carries `mark`.
    func isActive(_ mark: NoteMark) -> Bool {
        guard let index = focusedBlock.flatMap(index(of:)) else { return false }
        let block = blocks[index]
        if block.kind.hasText {
            return NoteInlineText.isActive(mark, in: block.text, selection: block.selection)
        }
        guard let row = focusedRow(in: block) else { return false }
        return NoteInlineText.isActive(mark, in: row.text, selection: row.selection)
    }

    /// Toggle `mark` over the focused block's selection.
    ///
    /// A code block has no marks — its content is literal — and a divider has
    /// no text, so both are no-ops rather than errors.
    func toggleMark(_ mark: NoteMark) {
        guard !isReadOnly, let index = focusedBlock.flatMap(index(of:)) else { return }
        // Copied out and written back rather than passed as two `inout`
        // arguments: `@Observable` turns `blocks` into a computed property,
        // and two writebacks through one of those alias each other.
        if blocks[index].kind.hasText {
            var text = blocks[index].text
            var selection = blocks[index].selection
            NoteInlineText.toggle(mark, in: &text, selection: &selection)
            blocks[index].text = text
            blocks[index].selection = selection
            return
        }
        guard blocks[index].kind.hasRows,
              let row = blocks[index].rows.firstIndex(where: { hasSelection($0) })
        else { return }
        var text = blocks[index].rows[row].text
        var selection = blocks[index].rows[row].selection
        NoteInlineText.toggle(mark, in: &text, selection: &selection)
        blocks[index].rows[row].text = text
        blocks[index].rows[row].selection = selection
    }

    /// The block that owns `id`, whether `id` names a block or one of its
    /// rows. Focus lands on whichever text field the caret is in; the mark
    /// buttons act on the block around it.
    func owningBlock(of id: UUID) -> UUID? {
        if blocks.contains(where: { $0.id == id }) { return id }
        return blocks.first { $0.rows.contains { $0.id == id } }?.id
    }

    private func focusedRow(in block: NoteEditorBlock) -> NoteEditorRow? {
        block.rows.first { hasSelection($0) }
    }

    private func hasSelection(_ row: NoteEditorRow) -> Bool {
        if case .ranges = row.selection.indices(in: row.text) { return true }
        return false
    }
}

import Foundation
import SwiftUI

/// What a block is, as a picker can offer it.
///
/// Flatter than ``NoteBlock``: the three heading levels are three entries
/// because that is how someone chooses one, and `ul` / `ol` are two because
/// nobody thinks of "ordered" as a checkbox on a list.
enum NoteBlockKind: String, CaseIterable, Identifiable, Sendable {
    case paragraph
    case heading1
    case heading2
    case heading3
    case bulleted
    case numbered
    case checklist
    case code
    case quote
    case divider

    var id: Self { self }

    var title: String {
        switch self {
        case .paragraph: "Text"
        case .heading1: "Heading 1"
        case .heading2: "Heading 2"
        case .heading3: "Heading 3"
        case .bulleted: "Bulleted list"
        case .numbered: "Numbered list"
        case .checklist: "Checklist"
        case .code: "Code"
        case .quote: "Quote"
        case .divider: "Divider"
        }
    }

    var symbol: String {
        switch self {
        case .paragraph: "text.alignleft"
        case .heading1, .heading2, .heading3: "textformat.size"
        case .bulleted: "list.bullet"
        case .numbered: "list.number"
        case .checklist: "checklist"
        case .code: "curlybraces"
        case .quote: "text.quote"
        case .divider: "minus"
        }
    }

    /// Whether this kind holds a list of rows rather than one run of text.
    var hasRows: Bool {
        switch self {
        case .bulleted, .numbered, .checklist: true
        case .paragraph, .heading1, .heading2, .heading3, .code, .quote, .divider: false
        }
    }

    /// Whether rows may be indented under one another. A checklist is flat:
    /// the grammar's `Checklist` has no `children`.
    var allowsNesting: Bool {
        switch self {
        case .bulleted, .numbered: true
        default: false
        }
    }

    /// Whether the kind carries one editable run of inline text.
    var hasText: Bool {
        switch self {
        case .paragraph, .heading1, .heading2, .heading3, .quote: true
        case .bulleted, .numbered, .checklist, .code, .divider: false
        }
    }

    fileprivate var headingLevel: NoteHeadingLevel? {
        switch self {
        case .heading1: .one
        case .heading2: .two
        case .heading3: .three
        default: nil
        }
    }

    fileprivate static func heading(_ level: NoteHeadingLevel) -> Self {
        switch level {
        case .one: .heading1
        case .two: .heading2
        case .three: .heading3
        }
    }
}

/// One row of a list or checklist.
struct NoteEditorRow: Identifiable, Equatable {
    let id: UUID
    var text: AttributedString
    var checked: Bool
    /// Nesting, zero-based. Only bulleted and numbered lists use it.
    var depth: Int
    var selection = AttributedTextSelection()

    init(
        id: UUID = UUID(),
        text: AttributedString = AttributedString(),
        checked: Bool = false,
        depth: Int = 0
    ) {
        self.id = id
        self.text = text
        self.checked = checked
        self.depth = depth
    }
}

/// One block, in the shape a SwiftUI form can bind to.
///
/// A struct with a `kind` and the fields every kind might need, rather than an
/// enum: SwiftUI binds to stored properties, and `Binding` into an enum's
/// associated value is a wrapper per case. The fields a kind does not use are
/// simply not read — ``noteBlock`` is the only thing that decides what a block
/// means, and it switches on `kind`.
struct NoteEditorBlock: Identifiable, Equatable {
    let id: UUID
    var kind: NoteBlockKind
    /// Paragraph, heading and quote content.
    var text: AttributedString
    /// List and checklist content.
    var rows: [NoteEditorRow]
    /// Code block content. Literal, so a plain `String`.
    var code: String
    /// Code block language hint. Empty means none.
    var language: String
    var selection = AttributedTextSelection()

    init(
        id: UUID = UUID(),
        kind: NoteBlockKind = .paragraph,
        text: AttributedString = AttributedString(),
        rows: [NoteEditorRow] = [],
        code: String = "",
        language: String = ""
    ) {
        self.id = id
        self.kind = kind
        self.text = text
        self.rows = rows
        self.code = code
        self.language = language
    }

    /// Whether the block would contribute nothing to a rendered note.
    ///
    /// A divider is never empty: it is content that happens to have no text.
    var isEmpty: Bool {
        switch kind {
        case .divider: false
        case .code: code.isEmpty
        case .bulleted, .numbered, .checklist:
            rows.allSatisfy { NoteInlineText.isEmpty($0.text) }
        default: NoteInlineText.isEmpty(text)
        }
    }

    // MARK: - Reading

    /// Build the editable form of a block the core decoded.
    init(from block: NoteBlock) {
        switch block {
        case let .paragraph(inline):
            self.init(kind: .paragraph, text: NoteInlineText.attributed(from: inline))
        case let .heading(level, inline):
            self.init(
                kind: NoteBlockKind.heading(level),
                text: NoteInlineText.attributed(from: inline)
            )
        case let .quote(inline):
            self.init(kind: .quote, text: NoteInlineText.attributed(from: inline))
        case let .list(ordered, items):
            self.init(
                kind: ordered ? .numbered : .bulleted,
                rows: Self.flatten(items, depth: 0)
            )
        case let .checklist(items):
            self.init(
                kind: .checklist,
                rows: items.map {
                    NoteEditorRow(
                        text: NoteInlineText.attributed(from: $0.inline),
                        checked: $0.checked
                    )
                }
            )
        case let .code(language, content):
            self.init(kind: .code, code: content, language: language ?? "")
        case .divider:
            self.init(kind: .divider)
        }
    }

    /// Nested list items become rows carrying a depth.
    ///
    /// A child that is not itself a list has nowhere to go — the grammar
    /// allows any block under a list item, and this editor offers indent and
    /// outdent, not arbitrary nesting. Such a child is dropped here and the
    /// document is then locked read-only, because ``NoteEditorModel`` checks
    /// that this conversion round-trips before it lets anyone type.
    private static func flatten(_ items: [NoteListItem], depth: Int) -> [NoteEditorRow] {
        var rows: [NoteEditorRow] = []
        for item in items {
            rows.append(NoteEditorRow(text: NoteInlineText.attributed(from: item.inline), depth: depth))
            for child in item.children {
                if case let .list(_, nested) = child {
                    rows.append(contentsOf: flatten(nested, depth: depth + 1))
                }
            }
        }
        return rows
    }

    // MARK: - Writing

    /// What this block means, in the vocabulary the core stores.
    var noteBlock: NoteBlock {
        switch kind {
        case .paragraph:
            .paragraph(inline: NoteInlineText.inline(from: text))
        case .heading1, .heading2, .heading3:
            .heading(
                level: kind.headingLevel ?? .one,
                inline: NoteInlineText.inline(from: text)
            )
        case .quote:
            .quote(inline: NoteInlineText.inline(from: text))
        case .bulleted, .numbered:
            .list(ordered: kind == .numbered, items: Self.items(from: rows, ordered: kind == .numbered))
        case .checklist:
            .checklist(
                items: rows.map {
                    NoteChecklistItem(
                        checked: $0.checked,
                        inline: NoteInlineText.inline(from: $0.text)
                    )
                }
            )
        case .code:
            .code(language: language.isEmpty ? nil : language, content: code)
        case .divider:
            .divider
        }
    }

    /// Rows carrying a depth become nested list items again.
    private static func items(from rows: [NoteEditorRow], ordered: Bool) -> [NoteListItem] {
        var cursor = 0
        return build(rows, cursor: &cursor, depth: 0, ordered: ordered)
    }

    private static func build(
        _ rows: [NoteEditorRow],
        cursor: inout Int,
        depth: Int,
        ordered: Bool
    ) -> [NoteListItem] {
        var items: [NoteListItem] = []
        while cursor < rows.count, rows[cursor].depth >= depth {
            let row = rows[cursor]
            cursor += 1
            var children: [NoteBlock] = []
            if cursor < rows.count, rows[cursor].depth > depth {
                let nested = build(rows, cursor: &cursor, depth: depth + 1, ordered: ordered)
                if !nested.isEmpty {
                    // A nested list inherits its parent's ordering. The
                    // grammar would allow a numbered list inside a bulleted
                    // one; indent and outdent cannot say which was meant, so
                    // this editor does not pretend to — and a note that does
                    // it opens read-only rather than being flattened.
                    children = [.list(ordered: ordered, items: nested)]
                }
            }
            items.append(
                NoteListItem(inline: NoteInlineText.inline(from: row.text), children: children)
            )
        }
        return items
    }
}

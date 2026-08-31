import Foundation
import SwiftUI

/// Inline note content, as the attributes SwiftUI's text editing understands.
///
/// The grammar in `docs/02-domain/notes.md` names five marks and nothing else
/// — no fonts, no colours, no sizes. So the conversion below is the schema
/// lock: it reads exactly five attributes off an `AttributedString` and
/// ignores everything a paste might have carried in. A user cannot produce a
/// body the grammar cannot express, because the only things that survive the
/// trip back out are the things the grammar names.
///
/// The marks map onto attributes SwiftUI already *renders*, rather than onto
/// private ones, which is what makes bold look bold while you type it:
/// `inlinePresentationIntent` carries bold, italic, strikethrough and code,
/// and `underlineStyle` carries the fifth.
enum NoteInlineText {
    // MARK: - Reading

    /// Render inline runs as editable text.
    ///
    /// A link rides on the standard link attribute, so it renders, stays
    /// clickable, and comes back out as a link.
    ///
    /// `ref`, `mention` and `redacted` runs render as their visible text and
    /// nothing more. They are not authorable — an entity link needs a picker
    /// this editor does not have, and a redaction is produced at egress, not
    /// by a person — so a note containing one opens read-only. See
    /// ``NoteEditorModel``.
    static func attributed(from inline: [NoteInline]) -> AttributedString {
        var out = AttributedString()
        for run in inline {
            out.append(piece(for: run))
        }
        return out
    }

    private static func piece(for run: NoteInline) -> AttributedString {
        switch run {
        case let .text(text, marks):
            var piece = AttributedString(text)
            piece.setAttributes(container(for: marks))
            return piece
        case let .link(href, label):
            var piece = AttributedString(label)
            piece.link = URL(string: href)
            return piece
        case let .ref(target):
            return AttributedString(target)
        case let .mention(person):
            return AttributedString("@\(person)")
        case let .redacted(_, placeholderText):
            return AttributedString(placeholderText)
        }
    }

    // MARK: - Writing

    /// Read editable text back as inline runs.
    ///
    /// Adjacent runs carrying the same marks are merged. `AttributedString`
    /// splits a run whenever *any* attribute changes, including ones this
    /// grammar does not model, and two `.text` runs where the document had
    /// one would read as a change the user never made.
    static func inline(from text: AttributedString) -> [NoteInline] {
        var out: [NoteInline] = []
        for run in text.runs {
            let slice = String(text[run.range].characters)
            guard !slice.isEmpty else { continue }
            if let url = run.link {
                out.append(.link(href: url.absoluteString, label: slice))
                continue
            }
            let marks = marks(in: run)
            if case let .text(previous, previousMarks) = out.last, previousMarks == marks {
                out[out.count - 1] = .text(text: previous + slice, marks: marks)
            } else {
                out.append(.text(text: slice, marks: marks))
            }
        }
        return out
    }

    /// The plain characters, for a length check or an empty test.
    static func isEmpty(_ text: AttributedString) -> Bool {
        text.characters.isEmpty
    }

    // MARK: - Marks

    /// The marks on one run, in the order the core sorts them.
    ///
    /// The order is not cosmetic: the editor decides whether a note has been
    /// changed by comparing the blocks it would write against the blocks it
    /// read, and the core emits marks sorted. A different order here would
    /// make every note look edited the moment it was opened.
    static func marks(in run: AttributedString.Runs.Run) -> [NoteMark] {
        var marks: [NoteMark] = []
        let intent = run.inlinePresentationIntent ?? []
        if intent.contains(.stronglyEmphasized) { marks.append(.bold) }
        if intent.contains(.emphasized) { marks.append(.italic) }
        if run.underlineStyle != nil { marks.append(.underline) }
        if intent.contains(.strikethrough) { marks.append(.strike) }
        if intent.contains(.code) { marks.append(.code) }
        return marks
    }

    /// The attributes that render `marks`.
    static func container(for marks: [NoteMark]) -> AttributeContainer {
        var container = AttributeContainer()
        var intent: InlinePresentationIntent = []
        if marks.contains(.bold) { intent.insert(.stronglyEmphasized) }
        if marks.contains(.italic) { intent.insert(.emphasized) }
        if marks.contains(.strike) { intent.insert(.strikethrough) }
        if marks.contains(.code) { intent.insert(.code) }
        if !intent.isEmpty { container.inlinePresentationIntent = intent }
        if marks.contains(.underline) { container.underlineStyle = .single }
        return container
    }

    /// Whether every run of `selection` already carries `mark`.
    ///
    /// "Every", not "any": a toolbar button that lit up for a partly-bold
    /// selection would then un-bold it, which is the opposite of what the
    /// person pressing it wants.
    static func isActive(
        _ mark: NoteMark,
        in text: AttributedString,
        selection: AttributedTextSelection
    ) -> Bool {
        guard case let .ranges(ranges) = selection.indices(in: text) else { return false }
        let slice = text[ranges]
        var sawRun = false
        for run in slice.runs {
            sawRun = true
            if !marks(in: run).contains(mark) { return false }
        }
        return sawRun
    }

    /// Turn `mark` on across the selection, or off if it is already on
    /// throughout.
    ///
    /// A collapsed selection is a no-op. Typing attributes — "make what I
    /// type next bold" — need a caret this editor does not own, and silently
    /// doing nothing beats silently marking the whole block.
    static func toggle(
        _ mark: NoteMark,
        in text: inout AttributedString,
        selection: inout AttributedTextSelection
    ) {
        guard case .ranges = selection.indices(in: text) else { return }
        let enable = !isActive(mark, in: text, selection: selection)
        text.transformAttributes(in: &selection) { container in
            apply(mark, enabled: enable, to: &container)
        }
    }

    private static func apply(
        _ mark: NoteMark,
        enabled: Bool,
        to container: inout AttributeContainer
    ) {
        if mark == .underline {
            container.underlineStyle = enabled ? .single : nil
            return
        }
        guard let flag = presentationIntent(for: mark) else { return }
        var intent = container.inlinePresentationIntent ?? []
        if enabled {
            intent.insert(flag)
        } else {
            intent.remove(flag)
        }
        container.inlinePresentationIntent = intent.isEmpty ? nil : intent
    }

    private static func presentationIntent(for mark: NoteMark) -> InlinePresentationIntent? {
        switch mark {
        case .bold: .stronglyEmphasized
        case .italic: .emphasized
        case .strike: .strikethrough
        case .code: .code
        case .underline: nil
        }
    }
}

extension NoteMark {
    /// Every mark, in the order the core sorts them — which is the order the
    /// toolbar shows them in, so the two never disagree.
    static let all: [NoteMark] = [.bold, .italic, .underline, .strike, .code]

    /// The toolbar's icon.
    var symbol: String {
        switch self {
        case .bold: "bold"
        case .italic: "italic"
        case .underline: "underline"
        case .strike: "strikethrough"
        case .code: "chevron.left.forwardslash.chevron.right"
        }
    }

    /// What the button says it does.
    var label: String {
        switch self {
        case .bold: "Bold"
        case .italic: "Italic"
        case .underline: "Underline"
        case .strike: "Strikethrough"
        case .code: "Code"
        }
    }

    /// The letter that toggles it with Command held.
    var shortcut: KeyEquivalent {
        switch self {
        case .bold: "b"
        case .italic: "i"
        case .underline: "u"
        case .strike: "x"
        case .code: "e"
        }
    }
}

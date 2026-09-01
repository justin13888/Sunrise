import Foundation

/// One row of the palette.
struct PaletteEntry: Identifiable, Hashable, Sendable {
    let action: AppAction
    /// Whether it can run right now. A command that needs a row and has none
    /// is shown greyed rather than hidden: the palette is also the app's answer
    /// to "what can this thing do", and a list that changes length depending on
    /// what is selected answers that question differently every time it is
    /// asked.
    let isEnabled: Bool

    var id: AppAction { action }
    var title: String { action.title }
    var symbol: String { action.symbol }
    /// The binding, printed beside the command — required of the palette by
    /// `docs/08-features/keyboard.md`. Empty for the handful of commands the
    /// spec gives no default chord.
    var shortcut: String { Keymap.shortcutLabel(for: action) }
}

/// Ranking a typed query against a command name.
///
/// A subsequence match, not a prefix one: people type `gti` for "Go to Inbox".
/// Pure and separate from the model so the ranking — the part that decides
/// whether Return runs the command someone meant — is testable on its own.
enum PaletteMatch {
    /// Higher is better; `nil` is no match at all.
    static func score(_ query: String, against title: String) -> Int? {
        let needle = Array(query.lowercased().filter { !$0.isWhitespace })
        guard !needle.isEmpty else { return 0 }
        let haystack = Array(title.lowercased())
        var score = 0
        var cursor = 0
        var lastMatch: Int?
        for character in needle {
            guard let found = haystack[cursor...].firstIndex(of: character) else { return nil }
            if found == 0 {
                score += 8
            } else if isWordStart(haystack, found) {
                score += 6
            }
            if let last = lastMatch, found == last + 1 { score += 4 }
            score += 1
            lastMatch = found
            cursor = found + 1
        }
        // A tie between "Open" and "Open Command Palette" goes to the shorter
        // name, which is the one the query described more completely.
        return score - haystack.count / 8
    }

    private static func isWordStart(_ characters: [Character], _ index: Int) -> Bool {
        guard index > 0 else { return true }
        return !characters[index - 1].isLetter && !characters[index - 1].isNumber
    }
}

/// The command palette: every action the app has, findable by typing.
///
/// It exists for two reasons and the second is the load-bearing one. It is a
/// fast way to run a command — and it is the *visible affordance* that
/// `docs/10-cross-cutting/accessibility.md` requires every keyboard shortcut to
/// have. A binding that appears nowhere on screen is a binding only the people
/// who read the manual have.
@MainActor
@Observable
final class CommandPaletteModel {
    var query: String = "" {
        didSet { highlighted = 0 }
    }

    private(set) var isPresented = false
    /// Where Return would land.
    private(set) var highlighted = 0

    /// Whether a row is under the list cursor, which decides what is runnable.
    private(set) var hasSelection = false

    init() {}

    /// Every command, in cheat-sheet order.
    ///
    /// The palette itself is left out: it is already open.
    static let catalogue: [AppAction] = KeySection.allCases.flatMap { section in
        AppAction.allCases.filter { $0.section == section && $0 != .commandPalette }
    }

    var results: [PaletteEntry] {
        let trimmed = query.trimmed
        let ranked = CommandPaletteModel.catalogue
            .compactMap { action -> (AppAction, Int)? in
                PaletteMatch.score(trimmed, against: action.title).map { (action, $0) }
            }
        // A stable sort on the catalogue order, so an empty query shows the
        // commands grouped the way the cheat sheet groups them rather than in
        // whatever order a hash gave.
        let ordered = trimmed.isEmpty
            ? ranked
            : ranked.enumerated()
                .sorted { left, right in
                    left.element.1 == right.element.1
                        ? left.offset < right.offset
                        : left.element.1 > right.element.1
                }
                .map(\.element)
        return ordered.map {
            PaletteEntry(action: $0.0, isEnabled: hasSelection || !$0.0.needsSelection)
        }
    }

    /// What Return would run, or `nil` when the query matches nothing runnable.
    var chosen: AppAction? {
        let rows = results
        guard rows.indices.contains(highlighted) else { return nil }
        let entry = rows[highlighted]
        return entry.isEnabled ? entry.action : nil
    }

    func present(hasSelection: Bool) {
        self.hasSelection = hasSelection
        query = ""
        highlighted = 0
        isPresented = true
    }

    func dismiss() {
        isPresented = false
        query = ""
        highlighted = 0
    }

    /// Move the highlight, clamped rather than wrapped. A palette that wrapped
    /// from the last row to the first would move the highlight off screen on a
    /// key that everywhere else means "further down".
    func moveHighlight(_ direction: SelectionMove) {
        let count = results.count
        guard count > 0 else {
            highlighted = 0
            return
        }
        switch direction {
        case .up: highlighted = max(0, highlighted - 1)
        case .down: highlighted = min(count - 1, highlighted + 1)
        }
    }
}

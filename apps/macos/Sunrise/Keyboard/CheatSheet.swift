import Foundation

/// What the keyboard can reach from where the user is standing.
///
/// The cheat sheet is specified as *contextual* — `?` in any view — so it has
/// to know what "here" is. Two facts decide it: whether a task list is on
/// screen at all, and whether vim mode is on.
struct KeyboardContext: Hashable, Sendable {
    /// Whether a list of rows is showing. Calendar, Focus and the two briefs
    /// have no rows, so the row keys are not merely inert there — they are not
    /// bound, and a sheet that listed them would be lying.
    var hasList: Bool = false
    /// Whether a row is under the cursor.
    var hasSelection: Bool = false
    var vimMode: Bool = false
}

struct CheatSheetRow: Identifiable, Hashable, Sendable {
    let title: String
    let keys: String
    var id: String { title + keys }
}

struct CheatSheetSection: Identifiable, Hashable, Sendable {
    let title: String
    let rows: [CheatSheetRow]
    var id: String { title }
}

/// The `?` sheet.
///
/// Built from the same `Keymap` the resolver reads, so a binding cannot be
/// listed here and absent from the app — the failure mode of every hand-written
/// shortcut list ever shipped.
enum CheatSheet {
    static func sections(for context: KeyboardContext) -> [CheatSheetSection] {
        // One table, grouped once. Grouping the two keymaps separately would
        // print "Help" twice — ⌘/ under one heading and `?` under an identical
        // one — which is how a reader learns to distrust the sheet.
        var table = Keymap.application
        if context.hasList { table += Keymap.list }
        var built = grouped(table)
        if context.vimMode, context.hasList {
            built.append(vimSection)
        }
        return built.filter { !$0.rows.isEmpty }
    }

    /// The vim motions this client honours. Printed as its own section rather
    /// than folded into the others, because they are only live with the
    /// setting on and a sheet that mixed them in would read as though `h` and
    /// `←` were both always bound.
    static var vimSection: CheatSheetSection {
        let single = VimKeymap.motions
            .sorted { $0.value.title < $1.value.title }
            .map { CheatSheetRow(title: $0.value.title, keys: $0.key.display) }
        let prefixed = VimKeymap.prefixed
            .sorted { $0.key < $1.key }
            .map { CheatSheetRow(title: $0.value.title, keys: $0.key) }
        return CheatSheetSection(title: "Vim mode", rows: single + prefixed)
    }

    /// Group a keymap by section, keeping each section's bindings in the order
    /// the spec's table lists them.
    private static func grouped(_ bindings: [KeyBinding]) -> [CheatSheetSection] {
        KeySection.allCases.compactMap { section in
            var seen: [AppAction] = []
            for binding in bindings where binding.action.section == section {
                if !seen.contains(binding.action) { seen.append(binding.action) }
            }
            guard !seen.isEmpty else { return nil }
            return CheatSheetSection(
                title: section.title,
                rows: seen.map { action in
                    CheatSheetRow(
                        title: action.title,
                        keys: bindings.filter { $0.action == action }
                            .map(\.chord.display)
                            .joined(separator: "  /  ")
                    )
                }
            )
        }
    }
}

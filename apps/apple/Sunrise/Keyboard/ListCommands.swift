import Foundation

/// The rows one sheet is about.
///
/// Identified by its own token rather than by the tasks in it, so that
/// scheduling the same row twice in a row opens the sheet twice.
struct TaskBatch: Identifiable {
    let id = UUID()
    let tasks: [TaskItem]

    init?(_ tasks: [TaskItem]) {
        guard !tasks.isEmpty else { return nil }
        self.tasks = tasks
    }
}

/// The sheets a row action is waiting on.
///
/// Owned by the window rather than by the list, because the command palette can
/// ask for "Schedule…" too — and a second copy of that sheet living inside the
/// list would mean the palette opened nothing.
@MainActor
@Observable
final class RowSheets {
    var editing: TaskItem?
    var scheduling: TaskBatch?
    var moving: TaskBatch?

    init() {}
}

/// The requests a list cannot grant itself.
///
/// Starting a focus session is a write the list can make; *showing* the Focus
/// screen afterwards is the window's business, and so are the palette, the
/// cheat sheet and the search view. Passing them as closures keeps the list
/// ignorant of navigation — which is why the same rows can be drawn by Today,
/// by a stream and by Search without any of them growing a special case.
struct ListEscapes {
    var showFocus: () -> Void = {}
    var openSearch: () -> Void = {}
    var openPalette: () -> Void = {}
    var openCheatSheet: () -> Void = {}
}

/// What a row-scoped action *does*.
///
/// One function, called from two places — the key handler on the list, and the
/// command palette. Two copies would be two answers to "what does `X` do", and
/// the palette's copy is the one nobody would notice had drifted.
///
/// Returns `false` for a press this layer has no use for, so the key handler can
/// let it fall through to SwiftUI rather than swallowing it.
@MainActor
enum ListCommand {
    /// Four layers, tried in order: move the cursor, open a sheet over the
    /// selection, write to the vault, or hand the request up to the window.
    /// Anything none of them claims falls through as `false` — those are the
    /// ⌘-chords the menu bar already carries.
    static func perform(
        _ action: AppAction,
        list: TaskListModel,
        selection: ListSelection,
        sheets: RowSheets,
        escapes: ListEscapes
    ) -> Bool {
        if moveCursor(action, selection: selection) { return true }
        let targets = list.rows(for: selection.targets)
        if openSheet(action, targets: targets, sheets: sheets) { return true }
        if write(action, list: list, targets: targets, escapes: escapes) { return true }
        return escape(action, escapes: escapes)
    }

    /// The keys that only move the cursor. They work on an empty list too —
    /// they simply do nothing there — so they never fall through.
    private static func moveCursor(_ action: AppAction, selection: ListSelection) -> Bool {
        switch action {
        case .moveUp: selection.move(.up)
        case .moveDown: selection.move(.down)
        case .listTop: selection.moveToEdge(.first)
        case .listBottom: selection.moveToEdge(.last)
        case .extendSelectionUp: selection.extend(.up)
        case .extendSelectionDown: selection.extend(.down)
        case .toggleSelection: selection.toggleAtCursor()
        case .closeDetail: selection.clearSelection()
        default: return false
        }
        return true
    }

    /// The keys that ask a question before writing anything.
    private static func openSheet(
        _ action: AppAction,
        targets: [TaskItem],
        sheets: RowSheets
    ) -> Bool {
        guard let batch = TaskBatch(targets) else { return false }
        switch action {
        case .openDetail: sheets.editing = batch.tasks.first
        case .schedule: sheets.scheduling = batch
        case .moveToStream: sheets.moving = batch
        default: return false
        }
        return true
    }

    /// The keys that write straight through.
    private static func write(
        _ action: AppAction,
        list: TaskListModel,
        targets: [TaskItem],
        escapes: ListEscapes
    ) -> Bool {
        guard let first = targets.first else { return false }
        switch action {
        case .markDone: Task { await list.complete(targets) }
        case .deferTask: Task { await list.defer_(targets, byDays: 1) }
        case .focusMode:
            // One task, even with several ticked. A focus session is a
            // commitment to one thing, which is the entire point of it.
            Task {
                await list.startFocus(first)
                escapes.showFocus()
            }
        default: return false
        }
        return true
    }

    /// The keys a list can only pass upward.
    private static func escape(_ action: AppAction, escapes: ListEscapes) -> Bool {
        switch action {
        case .searchInView: escapes.openSearch()
        case .commandPalette: escapes.openPalette()
        case .cheatSheet: escapes.openCheatSheet()
        default: return false
        }
        return true
    }
}

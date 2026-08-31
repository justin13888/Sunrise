import Foundation

/// The Undo and Redo menu items.
///
/// The stack lives in the core, not here: an inverse command is built from the
/// vault's rows *before* a write lands, and only the seam is in a position to
/// read them at that moment. This model holds two labels and calls two
/// methods.
///
/// What it will not do is offer an undo that does nothing. A delete writes a
/// tombstone the core cannot restore, so it never reaches the stack — and the
/// screens that delete say so in a confirmation, before the fact is useless.
@MainActor
@Observable
final class UndoModel {
    private(set) var state = UndoState(undoLabel: nil, redoLabel: nil)
    /// The last thing undone or redone, for a status line.
    private(set) var lastAction: String?
    private(set) var errorMessage: String?

    private let bridge: CoreBridge

    init(bridge: CoreBridge) {
        self.bridge = bridge
    }

    var canUndo: Bool { state.undoLabel != nil }
    var canRedo: Bool { state.redoLabel != nil }

    /// `Undo complete “Renew passport”`, or plain `Undo` when there is
    /// nothing. Naming the step is the difference between an undo the user
    /// trusts and one they try in order to find out what it does.
    var undoTitle: String {
        state.undoLabel.map { "Undo \($0)" } ?? "Undo"
    }

    var redoTitle: String {
        state.redoLabel.map { "Redo \($0)" } ?? "Redo"
    }

    func refresh() async {
        state = await bridge.undoState()
    }

    func undo() async {
        do {
            lastAction = try await bridge.undo().map { "Undid \($0)" }
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
        await refresh()
    }

    func redo() async {
        do {
            lastAction = try await bridge.redo().map { "Redid \($0)" }
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
        await refresh()
    }

    /// Follow the change stream so the menu titles track writes made
    /// anywhere in the app — including the ones this model did not start.
    func follow() async {
        await refresh()
        for await batch in await bridge.changes() {
            guard !batch.isClosed else { return }
            await refresh()
        }
    }

    func dismissLastAction() { lastAction = nil }
}

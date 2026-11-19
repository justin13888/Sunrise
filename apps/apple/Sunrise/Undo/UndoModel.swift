import Foundation

/// The Undo and Redo menu items.
///
/// The stack lives in the core, not here: an inverse command is built from the
/// vault's rows *before* a write lands, and only the seam is in a position to
/// read them at that moment. This model holds two labels and calls two
/// methods.
///
/// What it will not do is offer an undo that does nothing: a command with no
/// inverse never reaches the stack. A delete has one — the core restores the
/// tombstone (ADR-0065 §3) — so it is undone like any other write.
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
        state.undoLabel.map { L10n.Undo.undoNamed(action: $0) } ?? L10n.Undo.undo
    }

    var redoTitle: String {
        state.redoLabel.map { L10n.Undo.redoNamed(action: $0) } ?? L10n.Undo.redo
    }

    func refresh() async {
        state = await bridge.undoState()
    }

    /// Undo the newest step: a planner commit through `plan_undo`, anything
    /// else through the core's command stack. A planner undo that left
    /// fields alone, because they were written again since, says how many it
    /// restored ("Restored 4 of 5").
    func undo() async {
        do {
            lastAction = try await bridge.undo().map { undone in
                let action = L10n.Undo.undid(action: undone.label)
                guard undone.skipped > 0 else { return action }
                let partial = L10n.Undo.restoredSome(
                    restored: undone.restored,
                    total: undone.restored + undone.skipped
                )
                return "\(action). \(partial)"
            }
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
        await refresh()
    }

    func redo() async {
        do {
            lastAction = try await bridge.redo().map { L10n.Undo.redid(action: $0) }
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

/// What an undo did: the step's label and, for a planner commit, how many
/// fields it put back and how many it left because they were written again
/// since (`docs/08-features/planner.md` §Undo). A command step restores
/// whatever its inverse writes, so it reports nothing skipped.
struct UndoneStep: Equatable, Sendable {
    let label: String
    let restored: Int
    let skipped: Int
}

/// The order of this device's undo steps, across the two places they live.
///
/// A planner commit is undone by `plan_undo` against the inverse the core
/// stored with it, not by the command stack's inverse command, so it never
/// goes on that stack. Undo still has to take the newest step first,
/// whichever kind it is, so this keeps the interleaving: one entry per
/// recorded step, `command` for one the core's stack holds and `plan` for a
/// commit. Pure, so the interleaving is tested without a vault.
///
/// Redo replays only command steps. A planner commit has no redo: its diff
/// is spent, and committing it again would re-solve, which the planner never
/// does. So a planner commit, like any new step, clears the redo side, and
/// undoing one adds nothing to it.
struct UndoJournal: Equatable, Sendable {
    enum Step: Equatable, Sendable {
        /// A step on the core's command stack.
        case command
        /// A planner commit: its id for `plan_undo`, and its menu label.
        case plan(commitId: String, label: String)
    }

    /// Oldest first. Bounded like the core's stack, which holds 64.
    private(set) var undoSteps: [Step] = []
    /// How many command steps the core's redo stack holds that this device
    /// may still replay.
    private(set) var redoCount = 0

    static let depth = 64

    /// The core's stack took a command step.
    mutating func recordCommand() {
        push(.command)
    }

    /// A planner commit landed.
    mutating func recordPlan(commitId: String, label: String) {
        push(.plan(commitId: commitId, label: label))
    }

    /// The newest step, taken off for undoing. A command step moves to the
    /// redo side; a planner commit does not. `nil` when nothing is recorded,
    /// in which case the core's stack is asked anyway: it may hold steps
    /// older than this journal, which starts empty each launch.
    mutating func takeUndo() -> Step? {
        guard let step = undoSteps.popLast() else { return nil }
        if step == .command { redoCount += 1 }
        return step
    }

    /// The core's undo failed: the step is gone from both stacks, and the
    /// redo side never got it.
    mutating func undoFailed(_ step: Step) {
        if step == .command { redoCount = max(0, redoCount - 1) }
    }

    /// Whether a redo would replay a command step. False after a planner
    /// commit even when the core's own redo stack still holds steps: those
    /// were undone before the commit, and replaying one now would land it
    /// on top of a plan it never saw.
    var canRedo: Bool { redoCount > 0 }

    /// A redo replayed a command step, which is undoable again.
    mutating func redid() {
        guard redoCount > 0 else { return }
        redoCount -= 1
        undoSteps.append(.command)
        trim()
    }

    /// The newest step's label when it is a planner commit, else `nil`: the
    /// core's stack names its own.
    var planLabel: String? {
        if case let .plan(_, label)? = undoSteps.last { return label }
        return nil
    }

    private mutating func push(_ step: Step) {
        undoSteps.append(step)
        redoCount = 0
        trim()
    }

    private mutating func trim() {
        if undoSteps.count > Self.depth {
            undoSteps.removeFirst(undoSteps.count - Self.depth)
        }
    }
}

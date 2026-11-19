import Foundation

// The bridge's undo and planner pass-throughs, in a file of their own because
// `CoreBridge.swift` was past the length this project lints for. Still the
// actor's own methods: an extension of an actor is isolated to it, so these
// keep the single-owner guarantee the type documents, and ``UndoJournal``
// is only ever touched on the actor.
extension CoreBridge {
    // MARK: - Undo

    /// Reverse the most recent recorded step, whichever stack holds it: a
    /// planner commit through `plan_undo`, anything else through the
    /// command stack. `nil` when there is nothing.
    func undo() async throws -> UndoneStep? {
        let step = journal.takeUndo()
        switch step {
        case let .plan(commitId, label)?:
            let undone = try core.planUndo(commitId: commitId)
            return UndoneStep(
                label: label,
                restored: Int(undone.restored),
                skipped: undone.skipped.count
            )
        case .command?, nil:
            do {
                return try await core.undo().map { UndoneStep(label: $0, restored: 0, skipped: 0) }
            } catch {
                if let step { journal.undoFailed(step) }
                throw error
            }
        }
    }

    /// Replay the most recently undone command step. A planner commit has no
    /// redo (``UndoJournal``), so after one there is nothing to replay.
    func redo() async throws -> String? {
        guard journal.canRedo else { return nil }
        let label = try await core.redo()
        if label != nil { journal.redid() }
        return label
    }

    /// What the Undo and Redo menu items should say right now: the newest
    /// planner commit's label when that is the next step, and a redo only
    /// when one may be replayed.
    func undoState() -> UndoState {
        let core = core.undoState()
        return UndoState(
            undoLabel: journal.planLabel ?? core.undoLabel,
            redoLabel: journal.canRedo ? core.redoLabel : nil
        )
    }

    // MARK: - The planner

    /// What dragging to `intent` would do, solved by the core against the
    /// vault as it stands. Writes nothing. Off the main actor, because the
    /// bridge is an actor; the caller keeps at most one in flight.
    func planPreview(_ intent: PlanIntentItem) throws -> PlanDiffItem {
        try core.planPreview(intent: intent)
    }

    /// Commit exactly the previewed diff `diffId`, and record it as one undo
    /// step labelled `label`.
    func planCommit(_ diffId: String, label: String) throws -> PlanCommitItem {
        let commit = try core.planCommit(diffId: diffId)
        if commit.moved > 0 { journal.recordPlan(commitId: commit.commitId, label: label) }
        return commit
    }

    /// Forget a previewed diff: the gesture was cancelled.
    func planCancel(_ diffId: String) {
        try? core.planCancel(diffId: diffId)
    }
}

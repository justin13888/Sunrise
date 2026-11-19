import Foundation
import Testing

@testable import Sunrise

/// The triage queue against a real vault: what `Query::Triage` puts in it, and
/// what each of the four actions writes. The queue's rule and order are the
/// core's (ADR-0047 §3, tested there); what is asserted here is that the model
/// renders that answer and that each action reaches the command it names.
@MainActor
struct TriageModelTests {
    private static let dayMs: Int64 = 24 * 60 * 60 * 1000

    private func draft(_ title: String, dueAt: TimeValue? = nil) -> TaskDraftIn {
        TaskDraftIn(
            title: title,
            body: nil,
            streamId: nil,
            contexts: [],
            priority: nil,
            energy: nil,
            estimatedDurationS: nil,
            scheduledAt: nil,
            dueAt: dueAt,
            schedulingConstraints: [],
            assignee: nil,
            reminderLeadS: nil
        )
    }

    /// A task whose hard deadline passed yesterday, and one with no deadline.
    private func seed(_ vault: borrowing TestVault) async throws -> EntityRef {
        let now = Int64(await vault.bridge.nowMs())
        let late = try await vault.bridge.submit(
            .createTask(draft: draft("Renew the passport", dueAt: .instant(at: now - Self.dayMs)))
        )
        _ = try await vault.bridge.submit(.createTask(draft: draft("Someday, a boat")))
        return late.entity
    }

    private func task(_ id: EntityRef, in vault: borrowing TestVault) async throws -> TaskItem {
        guard case let .task(item) = try await vault.bridge.query(.entityById(id: id)) else {
            throw CancellationError()
        }
        return item
    }

    @Test
    func aMissedHardDeadlineIsInTheQueueAndAnUndatedTaskIsNot() async throws {
        let vault = try await TestVault()
        let late = try await seed(vault)
        let model = TriageModel(bridge: vault.bridge)

        await model.refresh()

        #expect(model.rows.map(\.task.id) == [late])
        #expect(model.rows.first?.lateness.state == .lateHard)
        #expect(model.rows.first?.lateness.since != nil, "the row says since when")
        #expect(model.errorMessage == nil)
        await vault.bridge.shutdown()
    }

    /// Keep is one undoable command: the task leaves the queue, and Undo
    /// brings it back.
    @Test
    func keepLeavesTheTaskAloneAndUndoReturnsIt() async throws {
        let vault = try await TestVault()
        let late = try await seed(vault)
        let model = TriageModel(bridge: vault.bridge)
        await model.refresh()
        model.selectAll()

        await model.keep(model.selected)

        #expect(model.rows.isEmpty)
        #expect(model.selection.isEmpty)
        #expect(try await task(late, in: vault).lateAcknowledgedAt != nil)
        #expect(try await vault.bridge.undo() == L10n.Triage.keep)
        await model.refresh()
        #expect(model.rows.map(\.task.id) == [late])
        await vault.bridge.shutdown()
    }

    @Test
    func dropCancelsTheTaskWithItsReason() async throws {
        let vault = try await TestVault()
        let late = try await seed(vault)
        let model = TriageModel(bridge: vault.bridge)
        await model.refresh()

        await model.drop([late], reason: "  Not travelling after all ")

        let dropped = try await task(late, in: vault)
        #expect(dropped.state == .cancelled)
        #expect(dropped.cancelReason == "Not travelling after all")
        #expect(model.rows.isEmpty)
        await vault.bridge.shutdown()
    }

    /// A blank reason is no reason, not an empty one.
    @Test
    func dropWithABlankReasonRecordsNone() async throws {
        let vault = try await TestVault()
        let late = try await seed(vault)
        let model = TriageModel(bridge: vault.bridge)
        await model.refresh()

        await model.drop([late], reason: "   ")

        let dropped = try await task(late, in: vault)
        #expect(dropped.state == .cancelled)
        #expect(dropped.cancelReason == nil)
        await vault.bridge.shutdown()
    }

    @Test
    func alreadyDoneCompletesTheTaskAtTheMomentGiven() async throws {
        let vault = try await TestVault()
        let late = try await seed(vault)
        let model = TriageModel(bridge: vault.bridge)
        await model.refresh()
        let at = model.now

        await model.markDone([late], at: at)

        let done = try await task(late, in: vault)
        #expect(done.state == .done)
        #expect(done.completedAt == .instant(at: Int64((at.timeIntervalSince1970 * 1000).rounded())))
        #expect(model.rows.isEmpty)
        #expect(model.errorMessage == nil)
        await vault.bridge.shutdown()
    }

    /// The core refuses a completion in the future, and the refusal is shown
    /// rather than swallowed; nothing is written.
    @Test
    func alreadyDoneInTheFutureIsRefusedAndShown() async throws {
        let vault = try await TestVault()
        let late = try await seed(vault)
        let model = TriageModel(bridge: vault.bridge)
        await model.refresh()

        await model.markDone([late], at: model.now.addingTimeInterval(86_400))

        #expect(model.errorMessage != nil)
        #expect(try await task(late, in: vault).state != .done)
        #expect(model.rows.map(\.task.id) == [late])
        await vault.bridge.shutdown()
    }

    @Test
    func deferMovesThePlanAndCountsIt() async throws {
        let vault = try await TestVault()
        let late = try await seed(vault)
        let model = TriageModel(bridge: vault.bridge)
        await model.refresh()

        await model.deferToNextFreeSlot([late])

        let deferred = try await task(late, in: vault)
        #expect(deferred.deferredCount == 1)
        #expect(deferred.scheduledAt != nil)
        #expect(model.errorMessage == nil)
        await vault.bridge.shutdown()
    }

    /// A ticked task that leaves the queue some other way — completed from a
    /// notification, say — leaves the selection too, so the next action does
    /// not name it and have the core refuse the whole command.
    @Test
    func theSelectionFollowsTheQueue() async throws {
        let vault = try await TestVault()
        let late = try await seed(vault)
        let model = TriageModel(bridge: vault.bridge)
        await model.refresh()
        model.selectAll()
        #expect(model.selected == [late])

        _ = try await vault.bridge.submit(.completeTask(id: late))
        await model.refresh()

        #expect(model.selection.isEmpty)
        #expect(model.selected.isEmpty)
        await vault.bridge.shutdown()
    }

    /// An action on nothing writes nothing.
    @Test
    func anActionOnNoTasksIsANoOp() async throws {
        let vault = try await TestVault()
        let late = try await seed(vault)
        let model = TriageModel(bridge: vault.bridge)
        await model.refresh()

        await model.keep([])

        #expect(model.rows.map(\.task.id) == [late])
        #expect(model.errorMessage == nil)
        await vault.bridge.shutdown()
    }
}

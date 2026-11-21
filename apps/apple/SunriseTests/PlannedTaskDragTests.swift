import Foundation
import Testing

@testable import Sunrise

/// A task dragged over the calendar grid, through the planner
/// (`docs/08-features/planner.md`): the drag previews `MoveTask` with ghosts,
/// and the drop commits that diff as one undo step, planning the task where
/// it was let go. It makes no block; the grid draws the task.
///
/// Every test plans on tomorrow, so nothing it moves is in the past, which
/// the planner holds fixed.
@MainActor
struct PlannedTaskDragTests {
    private func model(_ vault: borrowing TestVault) async -> CalendarModel {
        let model = CalendarModel(bridge: vault.bridge)
        await model.refresh()
        await model.step(1)
        return model
    }

    private func hour(_ model: CalendarModel, _ hours: Double) -> Int64 {
        model.windowStartMs + Int64(hours * 3_600_000)
    }

    private func ghosts(_ model: CalendarModel) -> [PlanGhost] {
        model.planner.ghosts(
            fromMs: model.dayStartMs(offset: 0),
            toMs: model.dayStartMs(offset: 1),
            timeZone: model.timeZone
        )
    }

    private func placedTask(_ model: CalendarModel, _ title: String) -> PlacedTask? {
        model.placedTasks(dayOffset: 0).first { $0.task.title == title }
    }

    private func create(
        _ vault: borrowing TestVault,
        _ title: String,
        estimateS: UInt64? = nil,
        at: TimeValue? = nil
    ) async throws -> EntityRef {
        let draft = TaskDraftIn(
            title: title,
            body: nil,
            streamId: nil,
            contexts: [],
            priority: nil,
            energy: nil,
            estimatedDurationS: estimateS,
            scheduledAt: at,
            dueAt: nil,
            schedulingConstraints: [],
            assignee: nil,
            reminderLeadS: nil
        )
        return try await vault.bridge.submit(.createTask(draft: draft)).entity
    }

    /// **A task dropped on the grid is planned there.** Held over the grid it
    /// is previewed as a `MoveTask` with a ghost as long as its estimate,
    /// writing nothing; the drop commits that diff, the grid draws the task
    /// where its ghost was, no block is made, and one undo takes it back off.
    @Test
    func aDroppedTaskIsPlannedWhereItsGhostWasAndOneUndoTakesItBack() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        let task = try await create(vault, "Draft the proposal", estimateS: 90 * 60)

        await model.planner.previewTask(task, toMs: hour(model, 14), timeZone: model.timeZone)

        let subject = try #require(ghosts(model).first { $0.item == task })
        #expect(subject.role == .subject)
        #expect(subject.isTask)
        #expect(subject.after == GridSpan(startMs: hour(model, 14), endMs: hour(model, 15.5)))
        #expect(model.planner.moves(task))
        #expect(model.tasks.isEmpty, "a preview writes nothing")

        await model.planner.dropTask(task, atMs: hour(model, 14), timeZone: model.timeZone)

        #expect(model.planner.plan == nil)
        let placed = try #require(placedTask(model, "Draft the proposal"))
        #expect(placed.startMs == hour(model, 14))
        #expect(placed.endMs == hour(model, 15.5))
        #expect(model.rows.isEmpty, "a task drop plans the task; it makes no block")

        let undo = UndoModel(bridge: vault.bridge)
        await undo.refresh()
        let label = L10n.Calendar.undoPlanMove(title: "Draft the proposal", count: 1)
        #expect(undo.undoTitle == L10n.Undo.undoNamed(action: label))
        await undo.undo()
        await model.refresh()
        #expect(model.tasks.isEmpty)
        await vault.bridge.shutdown()
    }

    /// A task the grid draws is dragged along it the same way, and keeps the
    /// kind of time it has: a floating 09:00 moved to 11:00 is a floating
    /// 11:00, not pinned to a zone. With no estimate it is drawn as long as
    /// the default duration.
    @Test
    func aTimedTaskDraggedAlongTheGridKeepsItsKind() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        let task = try await create(
            vault, "Call the bank", at: model.timeValue(ms: hour(model, 9), kind: .floating)
        )
        await model.refresh()
        #expect(placedTask(model, "Call the bank")?.endMs == hour(model, 9.5))

        await model.planner.previewTask(task, toMs: hour(model, 11), timeZone: model.timeZone)
        await model.planner.dropTask(task, atMs: hour(model, 11), timeZone: model.timeZone)

        let moved = try #require(placedTask(model, "Call the bank"))
        #expect(moved.startMs == hour(model, 11))
        #expect(BlockTimeKind(try #require(moved.task.scheduledAt)) == .floating)
        await vault.bridge.shutdown()
    }

    /// A task drag that leaves the grid takes its ghosts with it and writes
    /// nothing; a drop whose payload is not a task writes nothing either.
    @Test
    func aTaskDragThatLeavesOrCarriesNoTaskWritesNothing() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        let task = try await create(vault, "Draft the proposal")

        await model.planner.previewTask(task, toMs: hour(model, 14), timeZone: model.timeZone)
        #expect(!ghosts(model).isEmpty)
        model.planner.taskDragLeft()
        #expect(model.planner.plan == nil)
        #expect(ghosts(model).isEmpty)

        model.planner.willDropTask()
        await model.planner.dropTask(nil, atMs: hour(model, 14), timeZone: model.timeZone)
        await model.refresh()
        #expect(model.planner.plan == nil)
        #expect(model.tasks.isEmpty)
        await vault.bridge.shutdown()
    }

    /// The hover's "no longer over the grid" can arrive after the drop has
    /// begun; it must not cancel the plan the drop commits.
    @Test
    func leavingTheGridAfterTheDropBeganKeepsThePlan() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        let task = try await create(vault, "Draft the proposal")

        await model.planner.previewTask(task, toMs: hour(model, 14), timeZone: model.timeZone)
        model.planner.willDropTask()
        model.planner.taskDragLeft()
        #expect(model.planner.plan != nil)
        await model.planner.dropTask(task, atMs: hour(model, 14), timeZone: model.timeZone)

        #expect(placedTask(model, "Draft the proposal")?.startMs == hour(model, 14))
        await vault.bridge.shutdown()
    }
}

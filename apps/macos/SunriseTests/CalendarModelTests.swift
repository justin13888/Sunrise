import Foundation
import Testing

@testable import Sunrise

/// The calendar against a real vault: draw a block, drop a task on it, collide
/// two of them and resolve it.
@MainActor
struct CalendarModelTests {
    private func model(_ vault: borrowing TestVault) async -> CalendarModel {
        let model = CalendarModel(bridge: vault.bridge)
        await model.refresh()
        return model
    }

    private func hour(_ model: CalendarModel, _ offset: Int) -> Int64 {
        model.windowStartMs + Int64(offset) * 3_600_000
    }

    private func draft(_ title: String) -> TaskDraftIn {
        TaskDraftIn(
            title: title,
            body: nil,
            streamId: nil,
            contexts: [],
            priority: nil,
            energy: nil,
            estimatedDurationS: nil,
            scheduledAt: nil,
            dueAt: nil,
            schedulingConstraints: [],
            assignee: nil,
            reminderLeadS: nil
        )
    }

    @Test
    func aBlockDrawnOnTheGridComesBackOnIt() async throws {
        let vault = try await TestVault()
        let model = await model(vault)

        await model.createBlock(
            fromMs: hour(model, 9),
            toMs: hour(model, 10),
            title: "Deep work",
            kind: .zoned
        )

        #expect(model.rows.count == 1)
        let placed = model.placed(dayOffset: 0)
        #expect(placed.count == 1)
        #expect(placed[0].row.title == "Deep work")
        #expect(placed[0].startMs == hour(model, 9))
        await vault.bridge.shutdown()
    }

    /// Dropping a task creates a block with **no** title, on purpose: the core
    /// shadow-copies the bound task's title. A title composed here would be a
    /// second copy that could disagree with it.
    @Test
    func droppingATaskGivesTheBlockTheTasksOwnTitle() async throws {
        let vault = try await TestVault()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(draft("Draft the proposal"))
        let task = try #require(list.tasks.first)
        let model = await model(vault)

        await model.dropTask(task.id, fromMs: hour(model, 14), toMs: hour(model, 15), kind: .zoned)

        let row = try #require(model.rows.first)
        #expect(row.title == "Draft the proposal")
        #expect(row.block.tasks == [task.id])
        #expect(row.taskTitles == ["Draft the proposal"])
        await vault.bridge.shutdown()
    }

    /// The whole Resolve flow, and the one thing that must survive it: the
    /// union's time range.
    @Test
    func twoOverlappingBlocksAreShadedAndMergeIntoTheirUnion() async throws {
        let vault = try await TestVault()
        let model = await model(vault)

        await model.createBlock(
            fromMs: hour(model, 9), toMs: hour(model, 11), title: "Deep work", kind: .zoned
        )
        await model.createBlock(
            fromMs: hour(model, 10), toMs: hour(model, 13), title: "Design review", kind: .zoned
        )

        #expect(model.conflicts.count == 1)
        let conflict = try #require(model.conflicts.first)
        #expect(conflict.fromMs == hour(model, 10))
        #expect(conflict.toMs == hour(model, 11))
        #expect(model.shading(dayOffset: 0).count == 1)

        await model.merge(conflict)

        #expect(model.rows.count == 1, "both originals are tombstoned")
        #expect(model.conflicts.isEmpty)
        let placed = try #require(model.placed(dayOffset: 0).first)
        #expect(placed.startMs == hour(model, 9))
        #expect(placed.endMs == hour(model, 13))
        #expect(placed.row.title == "Deep work + Design review")
        await vault.bridge.shutdown()
    }

    /// Back-to-back is not a conflict — the rule that lives in the domain
    /// precisely so a client cannot decide otherwise.
    @Test
    func backToBackBlocksAreNotAConflict() async throws {
        let vault = try await TestVault()
        let model = await model(vault)

        await model.createBlock(
            fromMs: hour(model, 9), toMs: hour(model, 10), title: "Standup", kind: .zoned
        )
        await model.createBlock(
            fromMs: hour(model, 10), toMs: hour(model, 11), title: "Focus", kind: .zoned
        )

        #expect(model.rows.count == 2)
        #expect(model.conflicts.isEmpty)
        await vault.bridge.shutdown()
    }

    /// Dragging a block must not change what *kind* of commitment it is. A
    /// floating block moved an hour later is still floating; pinning it to a
    /// zone would silently decide that it no longer travels with the user.
    @Test
    func movingABlockPreservesItsTimeKind() async throws {
        let vault = try await TestVault()
        let model = await model(vault)

        await model.createBlock(
            fromMs: hour(model, 9), toMs: hour(model, 10), title: "Reading", kind: .floating
        )
        let before = try #require(model.rows.first)
        guard case .floating = before.block.startsAt else {
            Issue.record("the block was not created as floating")
            return
        }

        await model.moveBlock(before, toStartMs: hour(model, 15), toEndMs: hour(model, 16))

        let after = try #require(model.rows.first)
        guard case .floating = after.block.startsAt else {
            Issue.record("moving a floating block pinned it to a zone")
            return
        }
        #expect(model.placed(dayOffset: 0).first?.startMs == hour(model, 15))
        await vault.bridge.shutdown()
    }

    /// The three kinds are distinguishable after a round trip. If they were
    /// not, the picker would be decoration.
    @Test
    func eachTimeKindSurvivesTheRoundTrip() async throws {
        let vault = try await TestVault()
        let model = await model(vault)

        for (index, kind) in [BlockTimeKind.zoned, .floating, .instant].enumerated() {
            await model.createBlock(
                fromMs: hour(model, 8 + index * 2),
                toMs: hour(model, 9 + index * 2),
                title: kind.rawValue,
                kind: kind
            )
        }

        let byTitle = Dictionary(
            uniqueKeysWithValues: model.rows.map { ($0.title ?? "", $0.block.startsAt) }
        )
        #expect(BlockTimeKind(try #require(byTitle["zoned"])) == .zoned)
        #expect(BlockTimeKind(try #require(byTitle["floating"])) == .floating)
        #expect(BlockTimeKind(try #require(byTitle["instant"])) == .instant)
        await vault.bridge.shutdown()
    }

    @Test
    func unbindingATaskLeavesTheBlockStanding() async throws {
        let vault = try await TestVault()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(draft("Write the report"))
        let task = try #require(list.tasks.first)
        let model = await model(vault)
        await model.dropTask(task.id, fromMs: hour(model, 9), toMs: hour(model, 10), kind: .zoned)
        let block = try #require(model.rows.first).block.id

        await model.unbind(task: task.id, from: block)

        let row = try #require(model.rows.first)
        #expect(row.block.tasks.isEmpty)
        #expect(row.title == "Write the report", "the shadow copy survives the unbind")
        await vault.bridge.shutdown()
    }

    /// A week grid draws seven Monday-first columns, and `Query::WeekBlocks`
    /// is Monday-first too. A mismatch would put the right blocks in the wrong
    /// column, which reads as a sync failure.
    @Test
    func aWeekGridIsSevenMondayFirstDays() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        model.span = .week
        await model.refresh()

        #expect(model.windowEndMs - model.windowStartMs == 7 * 86_400_000)
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(identifier: model.timeZone) ?? .current
        let first = Date(timeIntervalSince1970: Double(model.windowStartMs) / 1000)
        #expect(calendar.component(.weekday, from: first) == 2, "Monday")
        await vault.bridge.shutdown()
    }

    /// Keep both is a documented no-op. It writes nothing and says so.
    @Test
    func keepingBothWritesNothing() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        await model.createBlock(
            fromMs: hour(model, 9), toMs: hour(model, 11), title: "One", kind: .zoned
        )
        await model.createBlock(
            fromMs: hour(model, 10), toMs: hour(model, 12), title: "Two", kind: .zoned
        )

        model.keepBoth()

        #expect(model.note != nil)
        #expect(model.rows.count == 2)
        #expect(model.conflicts.count == 1)
        await vault.bridge.shutdown()
    }
}

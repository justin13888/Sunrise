import Foundation
import Testing

@testable import Sunrise

/// What a drag is carrying, and what a drop target makes of it.
struct DropPayloadTests {
    /// A drop target verifies rather than guesses. Every draggable surface
    /// puts an `EntityRef`'s own text on the pasteboard, so anything that is
    /// not one is somebody else's drag passing over the window.
    @Test
    func onlyTheIdsOfTheRightKindAreTaken() {
        let items = ["tsk_01", "blk_09", "https://example.com", "tsk_02", ""]
        #expect(DropPayload.taskIDs(items) == ["tsk_01", "tsk_02"])
        #expect(DropPayload.blockIDs(items) == ["blk_09"])
        #expect(DropPayload.taskIDs([]).isEmpty)
        #expect(DropPayload.taskIDs(["Buy milk"]).isEmpty)
    }
}

/// The drops that write to the vault.
@MainActor
struct DropWritesTests {
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

    /// **Task → Stream.** The gesture `interaction-patterns.md` §Promote names
    /// beside the `m` key, and it must land the task in the same place.
    @Test
    func aTaskDroppedOnAStreamIsFiledThere() async throws {
        let vault = try await TestVault()
        let browse = BrowseModel(bridge: vault.bridge)
        await browse.createStream(name: "Work", color: .sky, cadence: nil)
        let stream = try #require(browse.streams.first { $0.name == "Work" })

        let outcome = try await vault.bridge.submit(.createTask(draft: draft("Renew passport")))
        let task = outcome.entity

        #expect(await browse.fileTasks([task], intoStream: stream.id))

        let inbox = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await inbox.refresh()
        #expect(inbox.tasks.isEmpty, "it left the Inbox")

        let filed = TaskListModel(bridge: vault.bridge, kind: .stream(id: stream.id, name: "Work"))
        await filed.refresh()
        #expect(filed.tasks.map(\.title) == ["Renew passport"])

        await vault.bridge.shutdown()
    }

    /// **Task → Context.** A context is a set membership, so a drop *adds*.
    /// Writing only the dropped context would strip every other one the task
    /// carried, which is what `TaskEdit.contexts` would do left to itself.
    @Test
    func aTaskDroppedOnAContextKeepsTheContextsItAlreadyHad() async throws {
        let vault = try await TestVault()
        let browse = BrowseModel(bridge: vault.bridge)
        await browse.createContext(name: "home", description: nil)
        await browse.createContext(name: "errands", description: nil)
        let home = try #require(browse.contexts.first { $0.name == "home" })
        let errands = try #require(browse.contexts.first { $0.name == "errands" })

        let outcome = try await vault.bridge.submit(.createTask(draft: draft("Buy milk")))
        let task = outcome.entity

        #expect(await browse.fileTasks([task], intoContext: home.id))
        #expect(await browse.fileTasks([task], intoContext: errands.id))

        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.refresh()
        let carried = try #require(list.tasks.first).contexts
        #expect(Set(carried) == [home.id, errands.id], "the first context survived the second drop")

        // A second drop of the same context writes nothing at all.
        #expect(!(await browse.fileTasks([task], intoContext: home.id)))

        await vault.bridge.shutdown()
    }

    /// A drop carrying nothing this target understands is declined, so the
    /// cursor says "no" rather than the drop landing on nothing.
    @Test
    func aDropWithNoTasksInItIsDeclined() async throws {
        let vault = try await TestVault()
        let browse = BrowseModel(bridge: vault.bridge)
        await browse.refresh()
        let inbox = try #require(browse.streams.first)

        #expect(!(await browse.fileTasks(["https://example.com"], intoStream: inbox.id)))
        #expect(!(await browse.fileTasks([], intoContext: "ctx_nope")))

        await vault.bridge.shutdown()
    }

    /// A model whose `UserDefaults` hold nothing, so whatever order it shows
    /// came from the vault.
    private func freshList(_ kind: TaskListKind, in vault: borrowing TestVault) throws -> TaskListModel {
        let defaults = try #require(UserDefaults(suiteName: "sunrise-tests-\(UUID().uuidString)"))
        return TaskListModel(
            bridge: vault.bridge,
            kind: kind,
            orderImport: ListOrderImport(defaults: defaults)
        )
    }

    /// **Task → Task.** The arrangement is written to the vault, so it
    /// survives a re-query and a second model with nothing of its own on this
    /// device sees it too — which is what lets it reach another device.
    @Test
    func reorderingAListWritesTheVault() async throws {
        let vault = try await TestVault()
        let list = try freshList(.inbox, in: vault)
        for title in ["One", "Two", "Three"] {
            _ = try await vault.bridge.submit(.createTask(draft: draft(title)))
        }
        await list.refresh()
        let queryOrder = list.tasks.map(\.title)
        #expect(queryOrder.count == 3)

        let last = try #require(list.tasks.last).id
        let first = try #require(list.tasks.first).id
        #expect(await list.reorder([last], before: first))
        let arranged = [queryOrder[2], queryOrder[0], queryOrder[1]]
        #expect(list.tasks.map(\.title) == arranged)
        #expect(
            list.tasks.map { $0.sortOrder != nil } == [true, false, false],
            "only the dragged row was written"
        )

        let elsewhere = try freshList(.inbox, in: vault)
        await elsewhere.refresh()
        #expect(
            elsewhere.tasks.map(\.title) == arranged,
            "an arrangement only one model can see is not in the vault"
        )

        // A drop that changes nothing submits nothing.
        #expect(!(await list.reorder([last], before: first)))

        await vault.bridge.shutdown()
    }

    /// **The one-time import.** What an earlier build kept in `UserDefaults`
    /// is written into the vault on the list's first refresh, and the stored
    /// copy is deleted.
    @Test
    func aStoredOrderIsImportedOnceAndForgotten() async throws {
        let vault = try await TestVault()
        var ids: [EntityRef] = []
        for title in ["One", "Two", "Three"] {
            ids.append(try await vault.bridge.submit(.createTask(draft: draft(title))).entity)
        }
        let defaults = try #require(UserDefaults(suiteName: "sunrise-tests-\(UUID().uuidString)"))
        defaults.set([ids[2], ids[0]], forKey: "list.order.inbox")
        let list = TaskListModel(
            bridge: vault.bridge,
            kind: .inbox,
            orderImport: ListOrderImport(defaults: defaults)
        )

        await list.refresh()
        #expect(list.tasks.map(\.title).prefix(2) == ["Three", "One"])
        #expect(defaults.stringArray(forKey: "list.order.inbox") == nil, "imported once, then deleted")

        let elsewhere = try freshList(.inbox, in: vault)
        await elsewhere.refresh()
        #expect(elsewhere.tasks.map(\.title).prefix(2) == ["Three", "One"])

        await vault.bridge.shutdown()
    }

    /// A list another device has arranged keeps that arrangement: the stored
    /// order is deleted on the first refresh, and nothing is written.
    @Test
    func aStoredOrderForAListArrangedElsewhereIsForgottenWithoutAWrite() async throws {
        let vault = try await TestVault()
        var ids: [EntityRef] = []
        for title in ["One", "Two", "Three"] {
            ids.append(try await vault.bridge.submit(.createTask(draft: draft(title))).entity)
        }
        // Another device's drag, as it arrives here: a placed row this
        // device's import did not place.
        _ = try await vault.bridge.submit(.moveTasks(ids: [ids[1]], after: nil, before: nil))
        let before = try freshList(.inbox, in: vault)
        await before.refresh()
        let arranged = before.tasks.map(\.id)
        let keys = before.tasks.map(\.sortOrder)

        let defaults = try #require(UserDefaults(suiteName: "sunrise-tests-\(UUID().uuidString)"))
        defaults.set([ids[2], ids[0]], forKey: "list.order.inbox")
        let list = TaskListModel(
            bridge: vault.bridge,
            kind: .inbox,
            orderImport: ListOrderImport(defaults: defaults)
        )
        await list.refresh()

        #expect(defaults.stringArray(forKey: "list.order.inbox") == nil, "nothing to write, so forgotten")
        #expect(list.tasks.map(\.id) == arranged)
        #expect(list.tasks.map(\.sortOrder) == keys, "no key was written")

        await vault.bridge.shutdown()
    }

    /// **Two lists sharing tasks.** The Inbox imports first and places a task
    /// a Context also holds. The Context's import then sees that placed row,
    /// which this device placed, and still writes the rest of its stored
    /// order after it rather than deleting it unwritten.
    @Test
    func aSecondListSharingTasksStillImportsItsOrder() async throws {
        let vault = try await TestVault()
        let browse = BrowseModel(bridge: vault.bridge)
        await browse.createContext(name: "home", description: nil)
        let home = try #require(browse.contexts.first { $0.name == "home" })
        var ids: [EntityRef] = []
        for title in ["One", "Two", "Three", "Four"] {
            ids.append(try await vault.bridge.submit(.createTask(draft: draft(title))).entity)
        }
        #expect(await browse.fileTasks([ids[1], ids[2], ids[3]], intoContext: home.id))

        let defaults = try #require(UserDefaults(suiteName: "sunrise-tests-\(UUID().uuidString)"))
        defaults.set([ids[0], ids[1]], forKey: "list.order.inbox")
        defaults.set([ids[3], ids[2]], forKey: "list.order.context.\(home.id)")
        let inbox = TaskListModel(
            bridge: vault.bridge,
            kind: .inbox,
            orderImport: ListOrderImport(defaults: defaults)
        )
        await inbox.refresh()
        #expect(inbox.tasks.map(\.title).prefix(2) == ["One", "Two"])

        let context = TaskListModel(
            bridge: vault.bridge,
            kind: .context(id: home.id, name: "home"),
            orderImport: ListOrderImport(defaults: defaults)
        )
        await context.refresh()
        #expect(
            context.tasks.map(\.title) == ["Two", "Four", "Three"],
            "the shared row keeps the Inbox's place"
        )
        #expect(context.tasks.allSatisfy { $0.sortOrder != nil }, "the Context's own order was written")
        #expect(defaults.stringArray(forKey: "list.order.context.\(home.id)") == nil)
        #expect(defaults.object(forKey: ListOrderImport.placedHereKey) == nil, "no stored order left")

        await vault.bridge.shutdown()
    }

    /// Today declines the drop rather than accepting a move it would lose on
    /// the next refresh.
    @Test
    func todayRefusesToBeArrangedByHand() async throws {
        let vault = try await TestVault()
        let list = TaskListModel(bridge: vault.bridge, kind: .todayAll)
        _ = try await vault.bridge.submit(.createTask(draft: draft("One")))
        _ = try await vault.bridge.submit(.createTask(draft: draft("Two")))
        await list.refresh()

        #expect(!list.acceptsReordering)
        #expect(list.reorderCommand(["tsk_whatever"], before: "tsk_other") == nil)
        #expect(!(await list.reorder(["tsk_whatever"], before: "tsk_other")))

        await vault.bridge.shutdown()
    }

    /// Binding a task to a block is the other direction of the drop the
    /// calendar grid already accepts.
    @Test
    func aTaskCanBeBoundToABlock() async throws {
        let vault = try await TestVault()
        let calendar = CalendarModel(bridge: vault.bridge)
        await calendar.refresh()
        let start = calendar.dayStartMs(offset: 0) + 9 * 3_600_000
        await calendar.createBlock(fromMs: start, toMs: start + 3_600_000,
                                   title: "Deep work", kind: .zoned)
        let block = try #require(calendar.rows.first).block.id

        let outcome = try await vault.bridge.submit(.createTask(draft: draft("Write the memo")))
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.bind(outcome.entity, to: block)

        await calendar.refresh()
        #expect(calendar.rows.first?.taskTitles == ["Write the memo"])

        await vault.bridge.shutdown()
    }
}

/// Moving and resizing a block already on the grid — the third gap
/// `parity-matrix.md` names.
struct BlockDragTests {
    private let day: Int64 = 1_772_409_600_000
    private var dayEnd: Int64 { day + 24 * 3_600_000 }

    private func apply(
        _ mode: BlockDragMode,
        from startMs: Int64,
        to endMs: Int64,
        by deltaMs: Int64,
        snap: Int = 15
    ) -> (startMs: Int64, endMs: Int64) {
        BlockDrag.apply(
            mode: mode,
            startMs: startMs,
            endMs: endMs,
            deltaMs: deltaMs,
            snapMinutes: snap,
            dayStartMs: day,
            dayEndMs: dayEnd
        )
    }

    /// A moved block keeps its length. Dragging a floating hour an hour later
    /// must not turn it into ninety minutes.
    @Test
    func movingKeepsTheLength() {
        let start = day + 9 * 3_600_000
        let moved = apply(.move, from: start, to: start + 3_600_000, by: 3_600_000)
        #expect(moved.startMs == day + 10 * 3_600_000)
        #expect(moved.endMs - moved.startMs == 3_600_000)
    }

    /// The translation is snapped, at whatever the user set the grid to.
    @Test
    func aMoveSnapsToTheGrid() {
        let start = day + 9 * 3_600_000
        let nudged = apply(.move, from: start, to: start + 3_600_000, by: 8 * 60_000)
        #expect(nudged.startMs == day + 9 * 3_600_000 + 15 * 60_000, "on a 15-minute grid, 8 rounds up")

        let fine = apply(.move, from: start, to: start + 3_600_000, by: 8 * 60_000, snap: 5)
        #expect(fine.startMs == day + 9 * 3_600_000 + 10 * 60_000, "on a 5-minute grid it lands on 10")
    }

    /// A block dragged off the bottom of the column stops at midnight with its
    /// length intact — clamping the two ends separately is how a dragged block
    /// arrives three minutes long.
    @Test
    func aBlockDraggedPastMidnightStopsAtItWithoutShrinking() {
        let start = day + 23 * 3_600_000
        let moved = apply(.move, from: start, to: start + 3_600_000, by: 5 * 3_600_000)
        #expect(moved.endMs == dayEnd)
        #expect(moved.endMs - moved.startMs == 3_600_000)
    }

    @Test
    func aBlockDraggedAboveMidnightStopsAtTheTopOfTheDay() {
        let start = day + 30 * 60_000
        let moved = apply(.move, from: start, to: start + 3_600_000, by: -5 * 3_600_000)
        #expect(moved.startMs == day)
        #expect(moved.endMs - moved.startMs == 3_600_000)
    }

    /// A resize holds the start still. That is the whole difference between
    /// the two gestures, and getting it wrong moves an appointment while the
    /// user thought they were lengthening it.
    @Test
    func resizingHoldsTheStartAndMovesTheEnd() {
        let start = day + 9 * 3_600_000
        let stretched = apply(.resizeEnd, from: start, to: start + 3_600_000, by: 30 * 60_000)
        #expect(stretched.startMs == start)
        #expect(stretched.endMs == start + 90 * 60_000)
    }

    /// Dragged shorter than a snap step, a block stops at one step rather than
    /// becoming a zero-length block the core would refuse and the grid could
    /// not draw.
    @Test
    func aResizeNeverShortensPastOneSnapStep() {
        let start = day + 9 * 3_600_000
        let squashed = apply(.resizeEnd, from: start, to: start + 3_600_000, by: -5 * 3_600_000)
        #expect(squashed.startMs == start)
        #expect(squashed.endMs == start + 15 * 60_000)
    }

    @Test
    func aResizeStopsAtMidnight() {
        let start = day + 23 * 3_600_000
        let stretched = apply(.resizeEnd, from: start, to: start + 30 * 60_000, by: 5 * 3_600_000)
        #expect(stretched.endMs == dayEnd)
    }

    /// A drag of nothing changes nothing, so a click that wobbles two points
    /// does not put a write on the undo stack.
    @Test
    func aZeroDragIsAZeroChange() {
        let start = day + 9 * 3_600_000
        let same = apply(.move, from: start, to: start + 3_600_000, by: 0)
        #expect(same.startMs == start)
        #expect(same.endMs == start + 3_600_000)
    }
}

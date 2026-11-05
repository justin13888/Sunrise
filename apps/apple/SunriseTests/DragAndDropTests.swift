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

/// Rows of a list as the core returns it, for the pure tests below.
private enum Rows {
    static let one = "tsk_01ARZ3NDEKTSV4RRFFQ69G5FAA"
    static let two = "tsk_01ARZ3NDEKTSV4RRFFQ69G5FAB"
    static let three = "tsk_01ARZ3NDEKTSV4RRFFQ69G5FAC"
    static let four = "tsk_01ARZ3NDEKTSV4RRFFQ69G5FAD"

    /// One row per `(id, key)`, in the order given.
    static func list(_ rows: [(String, String?)]) -> [TaskItem] {
        rows.map { id, key in
            var task = Fixture.task(id: id)
            task.sortOrder = key
            return task
        }
    }

    /// The three fields of a `moveTasks`, or `nil` for any other command.
    static func fields(_ command: CoreCommand?) -> MoveFields? {
        guard case let .moveTasks(ids, after, before) = command else { return nil }
        return MoveFields(ids: ids, after: after, before: before)
    }
}

private struct MoveFields: Equatable {
    let ids: [EntityRef]
    let after: EntityRef?
    let before: EntityRef?
}

/// A drop in a task list, as the `MoveTasks` the core plans for it.
struct ListOrderTests {
    /// The ordinary case: a placed list and one dragged row moves that row
    /// alone, between its two new neighbours.
    @Test
    func aRowDroppedOnAnotherLandsImmediatelyAboveIt() {
        let list = Rows.list([(Rows.one, "G"), (Rows.two, "N"), (Rows.three, "T")])
        #expect(
            Rows.fields(ListOrder.move([Rows.three], before: Rows.two, in: list))
                == MoveFields(ids: [Rows.three], after: Rows.one, before: Rows.two)
        )
        #expect(
            Rows.fields(ListOrder.move([Rows.three], before: Rows.one, in: list))
                == MoveFields(ids: [Rows.three], after: nil, before: Rows.one)
        )
    }

    /// A whole ⇧-selected run moves together and keeps the order it is in
    /// the list, whatever order the drag delivered it in.
    @Test
    func aMultiRowDragKeepsTheRowsInTheOrderTheyWereIn() {
        let list = Rows.list([(Rows.one, "G"), (Rows.two, "K"), (Rows.three, "N"), (Rows.four, "T")])
        #expect(
            Rows.fields(ListOrder.move([Rows.four, Rows.two], before: Rows.one, in: list))
                == MoveFields(ids: [Rows.two, Rows.four], after: nil, before: Rows.one)
        )
    }

    /// In a list nobody has arranged, a row cannot land below an unplaced
    /// one unless that row is placed too, so the core moves it with the drag.
    @Test
    func aDropBelowUnplacedRowsPlacesThemToo() {
        let list = Rows.list([(Rows.one, nil), (Rows.two, nil), (Rows.three, nil)])
        #expect(
            Rows.fields(ListOrder.move([Rows.three], before: Rows.two, in: list))
                == MoveFields(ids: [Rows.one, Rows.three], after: nil, before: nil)
        )
    }

    /// Dropping onto a row that is itself being dragged, or naming rows the
    /// list does not hold, is a no-op and not a write.
    @Test
    func aDropThatChangesNothingIsNoCommand() {
        let list = Rows.list([(Rows.one, "G"), (Rows.two, "N"), (Rows.three, "T")])
        #expect(ListOrder.move([Rows.two], before: Rows.two, in: list) == nil)
        #expect(ListOrder.move([Rows.one, Rows.two], before: Rows.one, in: list) == nil)
        #expect(ListOrder.move([Rows.one], before: Rows.two, in: list) == nil, "already there")
        #expect(ListOrder.move([], before: Rows.one, in: list) == nil)
        #expect(ListOrder.move([Rows.four], before: Rows.one, in: list) == nil)
        #expect(ListOrder.move([Rows.one], before: Rows.four, in: list) == nil)
    }

    /// Today is sectioned by urgency and Search is ranked by relevance. A row
    /// dragged in either would snap back on the next refresh, so both decline
    /// the drop rather than accepting one they cannot honour.
    @Test
    func theSectionedAndRankedListsCannotBeArrangedByHand() {
        #expect(!TaskListKind.todayAll.acceptsReordering)
        #expect(!TaskListKind.today(contexts: ["ctx_1"]).acceptsReordering)
        #expect(!TaskListKind.search(text: "milk").acceptsReordering)

        #expect(TaskListKind.inbox.acceptsReordering)
        #expect(TaskListKind.stream(id: "str_1", name: "Work").acceptsReordering)
        #expect(TaskListKind.context(id: "ctx_1", name: "home").acceptsReordering)
    }
}

/// The one-time import of the order an earlier build kept in `UserDefaults`.
@MainActor
struct ListOrderImportTests {
    private func scratchDefaults() throws -> UserDefaults {
        try #require(UserDefaults(suiteName: "sunrise-tests-\(UUID().uuidString)"))
    }

    /// The keys are the ones the earlier build wrote, so what it left behind
    /// is what gets read. Lists it never arranged have none.
    @Test
    func theStoredOrderIsReadFromWhereTheEarlierBuildKeptIt() throws {
        let defaults = try scratchDefaults()
        defaults.set([Rows.three, Rows.one], forKey: "list.order.stream.str_1")
        let store = ListOrderImport(defaults: defaults)

        #expect(store.stored(for: .stream(id: "str_1", name: "Work")) == [Rows.three, Rows.one])
        #expect(store.stored(for: .stream(id: "str_2", name: "Home")) == nil)
        #expect(store.stored(for: .inbox) == nil)
        #expect(TaskListKind.todayAll.legacyOrderKey == nil)
        #expect(TaskListKind.search(text: "milk").legacyOrderKey == nil)
        #expect(TaskListKind.context(id: "ctx_1", name: "home").legacyOrderKey == "list.order.context.ctx_1")
    }

    /// What the device kept goes ahead of the rest, in its order, as one move.
    @Test
    func aStoredOrderBecomesOneMove() throws {
        let defaults = try scratchDefaults()
        defaults.set([Rows.three, Rows.four, Rows.one], forKey: "list.order.inbox")
        let store = ListOrderImport(defaults: defaults)
        let list = Rows.list([(Rows.one, nil), (Rows.two, nil), (Rows.three, nil)])

        #expect(
            Rows.fields(store.command(for: .inbox, in: list))
                == MoveFields(ids: [Rows.three, Rows.one], after: nil, before: nil),
            "a row the list no longer holds is skipped"
        )
    }

    /// A list another device has arranged since holds a newer order than
    /// anything this device kept, and it is not overwritten.
    @Test
    func aListAlreadyArrangedIsLeftAlone() throws {
        let defaults = try scratchDefaults()
        defaults.set([Rows.three, Rows.one], forKey: "list.order.inbox")
        let store = ListOrderImport(defaults: defaults)
        let list = Rows.list([(Rows.one, "N"), (Rows.two, nil), (Rows.three, nil)])

        #expect(store.command(for: .inbox, in: list) == nil)

        store.forget(.inbox)
        #expect(store.stored(for: .inbox) == nil)
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

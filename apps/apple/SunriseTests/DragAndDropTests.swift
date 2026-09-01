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

/// Moving rows around in a list.
struct ListOrderTests {
    @Test
    func aRowDroppedOnAnotherLandsImmediatelyAboveIt() {
        let ids = ["a", "b", "c", "d"]
        #expect(ListOrder.moving(["d"], before: "b", in: ids) == ["a", "d", "b", "c"])
        #expect(ListOrder.moving(["a"], before: "d", in: ids) == ["b", "c", "a", "d"])
    }

    /// A whole ⇧-selected run moves together and keeps its internal order,
    /// which is what makes dragging five rows feel like dragging one thing.
    @Test
    func aMultiRowDragKeepsTheRowsInTheOrderTheyWereIn() {
        let ids = ["a", "b", "c", "d", "e"]
        #expect(ListOrder.moving(["d", "b"], before: "a", in: ids) == ["b", "d", "a", "c", "e"])
    }

    /// Dropping onto a row that is itself being dragged is a no-op, not a
    /// rearrangement of the selection.
    @Test
    func droppingOntoTheDraggedRowChangesNothing() {
        let ids = ["a", "b", "c"]
        #expect(ListOrder.moving(["b"], before: "b", in: ids) == ids)
        #expect(ListOrder.moving(["a", "b"], before: "a", in: ids) == ids)
        #expect(ListOrder.moving([], before: "a", in: ids) == ids)
        #expect(ListOrder.moving(["z"], before: "a", in: ids) == ids)
        #expect(ListOrder.moving(["a"], before: "z", in: ids) == ids)
        #expect(ListOrder.moving(["a"], before: nil, in: ids) == ids)
    }

    /// A list nobody has arranged comes back exactly as the core ordered it.
    @Test
    func noStoredOrderMeansQueryOrder() {
        #expect(ListOrder.applying([], to: ["a", "b"]) == ["a", "b"])
        #expect(ListOrder.applying(["x", "y"], to: ["a", "b"]) == ["a", "b"])
    }

    /// Rows the arrangement has never seen follow it rather than being
    /// scattered through it. A task captured a moment ago appearing in the
    /// middle of a hand-arranged run would look like somebody else had
    /// rewritten the arrangement.
    @Test
    func rowsTheOrderHasNeverSeenFollowTheOnesItHas() {
        #expect(ListOrder.applying(["c", "a"], to: ["a", "b", "c", "d"]) == ["c", "a", "b", "d"])
    }

    /// A row the query no longer returns is skipped, and is *not* forgotten —
    /// an undone delete has to come back where it was.
    @Test
    func aRowThatLeftTheQueryIsSkippedRatherThanBreakingTheOrder() {
        #expect(ListOrder.applying(["c", "b", "a"], to: ["a", "c"]) == ["c", "a"])
        #expect(ListOrder.applying(["c", "b", "a"], to: ["a", "b", "c"]) == ["c", "b", "a"])
    }
}

/// Where an arrangement is kept, and which lists have one at all.
@MainActor
struct ListOrderStoreTests {
    private func scratchDefaults() throws -> UserDefaults {
        try #require(UserDefaults(suiteName: "sunrise-tests-\(UUID().uuidString)"))
    }

    /// Today is sectioned by urgency and Search is ranked by relevance. A row
    /// dragged in either would snap back on the next refresh, so both decline
    /// the drop rather than accepting one they cannot honour.
    @Test
    func theSectionedAndRankedListsCannotBeArrangedByHand() {
        #expect(TaskListKind.todayAll.orderStorageKey == nil)
        #expect(TaskListKind.today(contexts: ["ctx_1"]).orderStorageKey == nil)
        #expect(TaskListKind.search(text: "milk").orderStorageKey == nil)
        #expect(!TaskListKind.todayAll.acceptsReordering)

        #expect(TaskListKind.inbox.acceptsReordering)
        #expect(TaskListKind.stream(id: "str_1", name: "Work").acceptsReordering)
        #expect(TaskListKind.context(id: "ctx_1", name: "home").acceptsReordering)
    }

    /// Two streams keep two arrangements, not one shared one.
    @Test
    func eachListRemembersItsOwnArrangement() throws {
        let store = ListOrderStore(defaults: try scratchDefaults())
        let work = TaskListKind.stream(id: "str_1", name: "Work")
        let home = TaskListKind.stream(id: "str_2", name: "Home")

        #expect(store.move(["c"], before: "a", in: work, visible: ["a", "b", "c"]))
        #expect(store.order(for: work) == ["c", "a", "b"])
        #expect(store.order(for: home).isEmpty)
    }

    @Test
    func anArrangementSurvivesARequeryAndCanBeReset() throws {
        let store = ListOrderStore(defaults: try scratchDefaults())
        let kind = TaskListKind.inbox
        store.move(["c"], before: "a", in: kind, visible: ["a", "b", "c"])

        // The same rows back from the core in the core's own order.
        #expect(ListOrder.applying(store.order(for: kind), to: ["a", "b", "c"]) == ["c", "a", "b"])

        store.reset(kind)
        #expect(store.order(for: kind).isEmpty)
    }

    /// A drop that changes nothing writes nothing, so the caller can skip the
    /// re-read — and a list that cannot be arranged writes nothing either.
    @Test
    func aDropThatChangesNothingIsReportedAsSuch() throws {
        let store = ListOrderStore(defaults: try scratchDefaults())
        #expect(!store.move(["a"], before: "a", in: .inbox, visible: ["a", "b"]))
        #expect(!store.move(["b"], before: "a", in: .todayAll, visible: ["a", "b"]))
        #expect(store.order(for: .inbox).isEmpty)
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

    /// **Task → Task.** The arrangement survives a re-query against the same
    /// vault, which is the only thing that makes it a reorder rather than a
    /// flicker.
    @Test
    func reorderingAListSurvivesARefresh() async throws {
        let vault = try await TestVault()
        let defaults = try #require(UserDefaults(suiteName: "sunrise-tests-\(UUID().uuidString)"))
        let list = TaskListModel(
            bridge: vault.bridge,
            kind: .inbox,
            order: ListOrderStore(defaults: defaults)
        )
        for title in ["One", "Two", "Three"] {
            _ = try await vault.bridge.submit(.createTask(draft: draft(title)))
        }
        await list.refresh()
        let queryOrder = list.tasks.map(\.title)
        #expect(queryOrder.count == 3)

        let last = try #require(list.tasks.last).id
        let first = try #require(list.tasks.first).id
        #expect(list.reorder([last], before: first))
        #expect(list.tasks.map(\.title) == [queryOrder[2], queryOrder[0], queryOrder[1]])

        await list.refresh()
        #expect(
            list.tasks.map(\.title) == [queryOrder[2], queryOrder[0], queryOrder[1]],
            "an arrangement that a refresh undoes is not an arrangement"
        )

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
        #expect(!list.reorder(["tsk_whatever"], before: "tsk_other"))

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

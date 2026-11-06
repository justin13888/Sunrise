import Foundation
import Testing

@testable import Sunrise

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

    /// A task has one place in every list that holds it, so the first list
    /// this device imports places rows another list holds too. Those rows are
    /// this device's own arrangement, not another device's, and the second
    /// list's stored order is placed after them rather than thrown away.
    @Test
    func rowsThisDevicePlacedDoNotStopTheNextImport() throws {
        let defaults = try scratchDefaults()
        defaults.set([Rows.four, Rows.two, Rows.three], forKey: "list.order.context.ctx_1")
        let store = ListOrderImport(defaults: defaults)
        store.recordPlaced(.moveTasks(ids: [Rows.one], after: nil, before: nil))
        let list = Rows.list([(Rows.one, "G"), (Rows.two, nil), (Rows.three, nil), (Rows.four, nil)])

        #expect(
            Rows.fields(store.command(for: .context(id: "ctx_1", name: "home"), in: list))
                == MoveFields(ids: [Rows.four, Rows.two, Rows.three], after: Rows.one, before: nil)
        )
    }

    /// One placed row this device did not place means another device has
    /// arranged the list, and nothing is written over it.
    @Test
    func aRowPlacedElsewhereStillLeavesTheListAlone() throws {
        let defaults = try scratchDefaults()
        defaults.set([Rows.three, Rows.four], forKey: "list.order.inbox")
        let store = ListOrderImport(defaults: defaults)
        store.recordPlaced(.moveTasks(ids: [Rows.one], after: nil, before: nil))
        let list = Rows.list([(Rows.one, "G"), (Rows.two, "N"), (Rows.three, nil), (Rows.four, nil)])

        #expect(store.command(for: .inbox, in: list) == nil)
    }

    /// Stored rows this device has already placed have nothing left to write.
    @Test
    func aStoredOrderWhoseRowsAreAllPlacedHasNothingToWrite() throws {
        let defaults = try scratchDefaults()
        defaults.set([Rows.two, Rows.one], forKey: "list.order.inbox")
        let store = ListOrderImport(defaults: defaults)
        store.recordPlaced(.moveTasks(ids: [Rows.one, Rows.two], after: nil, before: nil))
        let list = Rows.list([(Rows.one, "G"), (Rows.two, "N"), (Rows.three, nil)])

        #expect(store.command(for: .inbox, in: list) == nil)
    }

    /// The record of what the imports placed outlives each stored order but
    /// not the last of them.
    @Test
    func theRecordOfPlacedRowsGoesWithTheLastStoredOrder() throws {
        let defaults = try scratchDefaults()
        defaults.set([Rows.one], forKey: "list.order.inbox")
        defaults.set([Rows.two], forKey: "list.order.stream.str_1")
        let store = ListOrderImport(defaults: defaults)
        store.recordPlaced(.moveTasks(ids: [Rows.one], after: nil, before: nil))

        store.forget(.inbox)
        #expect(store.placedHere == [Rows.one])
        store.forget(.stream(id: "str_1", name: "Work"))
        #expect(store.placedHere.isEmpty)
        #expect(defaults.object(forKey: ListOrderImport.placedHereKey) == nil)
    }
}

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

    /// **A drag before an import.** A row dragged in the Inbox is placed in
    /// every list that holds it, and this device placed it. A Context holding
    /// it that has not imported its stored order yet still writes that order,
    /// after the dragged row, rather than taking the drag for another
    /// device's arrangement and deleting the order unwritten.
    @Test
    func aDragDoesNotStopAnotherListImportingItsOrder() async throws {
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
        defaults.set([ids[3], ids[2]], forKey: "list.order.context.\(home.id)")
        let inbox = TaskListModel(
            bridge: vault.bridge,
            kind: .inbox,
            orderImport: ListOrderImport(defaults: defaults)
        )
        await inbox.refresh()
        let first = try #require(inbox.tasks.first).id
        #expect(await inbox.reorder([ids[3]], before: first))
        #expect(inbox.errorMessage == nil)
        #expect(inbox.tasks.first?.id == ids[3])
        #expect(ListOrderImport(defaults: defaults).placedHere.contains(ids[3]), "the drag was recorded")

        let context = TaskListModel(
            bridge: vault.bridge,
            kind: .context(id: home.id, name: "home"),
            orderImport: ListOrderImport(defaults: defaults)
        )
        await context.refresh()
        #expect(
            context.tasks.map(\.title) == ["Four", "Three", "Two"],
            "the dragged row keeps its place and the stored order follows it"
        )
        #expect(
            context.tasks.first { $0.id == ids[2] }?.sortOrder != nil,
            "the Context's own order was written"
        )
        #expect(defaults.stringArray(forKey: "list.order.context.\(home.id)") == nil)
        #expect(defaults.object(forKey: ListOrderImport.placedHereKey) == nil, "no stored order left")

        await vault.bridge.shutdown()
    }

    /// A drag the core refuses placed nothing, so nothing is recorded as
    /// placed here.
    @Test
    func aRefusedDragIsNotRecorded() async throws {
        let vault = try await TestVault()
        var ids: [EntityRef] = []
        for title in ["One", "Two"] {
            ids.append(try await vault.bridge.submit(.createTask(draft: draft(title))).entity)
        }
        let defaults = try #require(UserDefaults(suiteName: "sunrise-tests-\(UUID().uuidString)"))
        defaults.set([ids[0]], forKey: "list.order.context.ctx_elsewhere")
        let inbox = TaskListModel(
            bridge: vault.bridge,
            kind: .inbox,
            orderImport: ListOrderImport(defaults: defaults)
        )
        await inbox.refresh()
        let first = try #require(inbox.tasks.first).id
        let last = try #require(inbox.tasks.last).id
        // Deleted under the list, which has not re-read it yet.
        _ = try await vault.bridge.submit(.deleteTask(id: last))

        #expect(await inbox.reorder([last], before: first))
        #expect(inbox.errorMessage != nil, "the refusal is reported")
        #expect(ListOrderImport(defaults: defaults).placedHere.isEmpty)

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

    /// **Calendar block → Task.** A block dropped on a task row binds the
    /// task; dropped again on the same row, now bound, it unbinds it.
    @Test
    func aBlockDroppedOnATaskBindsItAndDroppedAgainUnbindsIt() async throws {
        let vault = try await TestVault()
        let calendar = CalendarModel(bridge: vault.bridge)
        await calendar.refresh()
        let start = calendar.dayStartMs(offset: 0) + 9 * 3_600_000
        await calendar.createBlock(fromMs: start, toMs: start + 3_600_000,
                                   title: "Deep work", kind: .zoned)
        let block = try #require(calendar.rows.first).block.id
        _ = try await vault.bridge.submit(.createTask(draft: draft("Write the memo")))
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.refresh()

        await list.toggleBinding(try #require(list.tasks.first), block: block)
        #expect(list.tasks.first?.blocks == [block])
        await calendar.refresh()
        #expect(calendar.rows.first?.taskTitles == ["Write the memo"])

        await list.toggleBinding(try #require(list.tasks.first), block: block)
        #expect(list.tasks.first?.blocks.isEmpty == true)
        await calendar.refresh()
        #expect(calendar.rows.first?.block.tasks.isEmpty == true)

        await vault.bridge.shutdown()
    }
}

/// Moving and resizing a block on the grid through the planner
/// (`docs/08-features/planner.md`): the client sends the gesture, draws the
/// diff it gets back, and commits exactly that diff on drop.
///
/// Every test plans on tomorrow, so nothing it moves is in the past, which
/// the planner holds fixed.
@MainActor
struct PlannedDragTests {
    private func model(_ vault: borrowing TestVault) async -> CalendarModel {
        let model = CalendarModel(bridge: vault.bridge)
        await model.refresh()
        await model.step(1)
        return model
    }

    private func hour(_ model: CalendarModel, _ hours: Double) -> Int64 {
        model.windowStartMs + Int64(hours * 3_600_000)
    }

    private func row(_ model: CalendarModel, _ title: String) throws -> BlockGridRow {
        try #require(model.rows.first { $0.title == title })
    }

    private func start(_ model: CalendarModel, _ title: String) throws -> Int64 {
        timeValueMs(value: try row(model, title).block.startsAt, tz: model.timeZone)
    }

    private func end(_ model: CalendarModel, _ title: String) throws -> Int64 {
        timeValueMs(value: try row(model, title).block.endsAt, tz: model.timeZone)
    }

    /// Standup 9–10 and Focus 10–11, both flexible: what Sunrise creates.
    private func twoBlocks(_ model: CalendarModel) async {
        await model.createBlock(fromMs: hour(model, 9), toMs: hour(model, 10), title: "Standup", kind: .zoned)
        await model.createBlock(fromMs: hour(model, 10), toMs: hour(model, 11), title: "Focus", kind: .zoned)
    }

    /// **The ripple, drawn and then committed as drawn.** Standup dragged
    /// onto Focus moves Focus out of its way. The preview draws a ghost for
    /// each, writes nothing, and names the consequence; the drop commits the
    /// same positions, and one undo puts both back.
    @Test
    func aDropOntoAnotherBlockRipplesItAndOneUndoRestoresBoth() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        await twoBlocks(model)
        let standup = try row(model, "Standup")
        let focus = try row(model, "Focus").block.id
        #expect(!(try row(model, "Focus").isFixed), "a block Sunrise made is flexible")

        await model.previewDrag(standup, mode: .move, toMs: hour(model, 10))

        let plan = try #require(model.plan)
        #expect(plan.committable)
        #expect(Set(plan.moves.map(\.item)) == [standup.block.id, focus])
        let ghosts = model.ghosts(dayOffset: 0)
        #expect(ghosts.count == 2)
        let rippled = try #require(ghosts.first { $0.item == focus })
        #expect(rippled.role == .rippled)
        #expect(rippled.before == GridSpan(startMs: hour(model, 10), endMs: hour(model, 11)))
        let ghostOfFocus = try #require(rippled.after)
        #expect(model.planSummary?.hasPrefix(L10n.Calendar.planMoves(count: 2)) == true)
        #expect(try start(model, "Standup") == hour(model, 9), "a preview writes nothing")

        await model.dropDrag(standup, mode: .move, toMs: hour(model, 10))

        #expect(model.plan == nil)
        #expect(try start(model, "Standup") == hour(model, 10))
        #expect(try start(model, "Focus") == ghostOfFocus.startMs, "Focus landed where its ghost was")
        #expect(try end(model, "Focus") == ghostOfFocus.endMs)

        let undo = UndoModel(bridge: vault.bridge)
        await undo.refresh()
        #expect(undo.undoTitle == L10n.Undo.undoNamed(action: L10n.Calendar.undoPlanMove(title: "Standup", count: 2)))
        await undo.undo()
        await model.refresh()
        #expect(try start(model, "Standup") == hour(model, 9))
        #expect(try start(model, "Focus") == hour(model, 10))
        #expect(try end(model, "Focus") == hour(model, 11))

        await undo.refresh()
        #expect(!undo.canRedo, "a planner commit has no redo")
        await vault.bridge.shutdown()
    }

    /// A resize is planned too: the end moves, the start stays.
    @Test
    func aResizeMovesTheEndAndKeepsTheStart() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        await model.createBlock(fromMs: hour(model, 9), toMs: hour(model, 10), title: "Reading", kind: .floating)

        await model.dropDrag(try row(model, "Reading"), mode: .resizeEnd, toMs: hour(model, 11.5))

        #expect(try start(model, "Reading") == hour(model, 9))
        #expect(try end(model, "Reading") == hour(model, 11.5))
        guard case .floating = try row(model, "Reading").block.endsAt else {
            Issue.record("resizing a floating block pinned it to a zone")
            return
        }
        await vault.bridge.shutdown()
    }

    /// Dropped back where it started, a drag is a cancel: nothing is written
    /// and nothing goes on the undo stack.
    @Test
    func aDropBackAtTheOriginCommitsNothing() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        await twoBlocks(model)
        let standup = try row(model, "Standup")
        let undo = UndoModel(bridge: vault.bridge)
        await undo.refresh()
        let before = undo.undoTitle

        await model.previewDrag(standup, mode: .move, toMs: hour(model, 10))
        #expect(model.plan != nil)
        await model.dropDrag(standup, mode: .move, toMs: hour(model, 9))

        #expect(model.plan == nil)
        #expect(try start(model, "Standup") == hour(model, 9))
        await undo.refresh()
        #expect(undo.undoTitle == before)
        await vault.bridge.shutdown()
    }

    /// Esc clears the plan on screen and writes nothing.
    @Test
    func cancellingClearsTheGhostsAndWritesNothing() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        await twoBlocks(model)
        let standup = try row(model, "Standup")

        await model.previewDrag(standup, mode: .move, toMs: hour(model, 10))
        #expect(!model.ghosts(dayOffset: 0).isEmpty)
        model.cancelPlan()

        #expect(model.plan == nil)
        #expect(model.ghosts(dayOffset: 0).isEmpty)
        #expect(model.planSummary == nil)
        await model.refresh()
        #expect(try start(model, "Focus") == hour(model, 10))
        await vault.bridge.shutdown()
    }

    /// **Keyboard parity.** ⌥↓ twice previews the block two snaps later, the
    /// same plan a drag there would; Return commits it.
    @Test
    func theNudgeKeysPreviewAndReturnCommits() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        model.snapMinutes = 15
        await model.createBlock(fromMs: hour(model, 9), toMs: hour(model, 10), title: "Standup", kind: .zoned)

        #expect(await model.plan(.later, on: try row(model, "Standup")))
        #expect(await model.plan(.later, on: try row(model, "Standup")))
        let subject = try #require(model.plan?.moves.first)
        guard case let .slot(start, _) = subject.after else {
            Issue.record("the nudge did not preview a slot")
            return
        }
        #expect(timeValueMs(value: start, tz: model.timeZone) == hour(model, 9.5))
        #expect(try self.start(model, "Standup") == hour(model, 9), "nothing written before Return")

        #expect(await model.plan(.commit, on: try row(model, "Standup")))
        #expect(try self.start(model, "Standup") == hour(model, 9.5))
        #expect(model.nudge == nil)
        await vault.bridge.shutdown()
    }

    /// The resize keys move the end, and Esc drops the nudge.
    @Test
    func theResizeKeysMoveTheEndAndEscapeDropsThem() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        model.snapMinutes = 30
        await model.createBlock(fromMs: hour(model, 9), toMs: hour(model, 10), title: "Standup", kind: .zoned)

        #expect(await model.plan(.longer, on: try row(model, "Standup")))
        let subject = try #require(model.plan?.moves.first)
        guard case let .slot(start, end) = subject.after else {
            Issue.record("the nudge did not preview a slot")
            return
        }
        #expect(timeValueMs(value: start, tz: model.timeZone) == hour(model, 9))
        #expect(timeValueMs(value: end, tz: model.timeZone) == hour(model, 10.5))

        #expect(await model.plan(.cancel, on: try row(model, "Standup")))
        #expect(model.plan == nil)
        #expect(!(await model.plan(.commit, on: try row(model, "Standup"))), "nothing left to commit")
        #expect(try self.end(model, "Standup") == hour(model, 10))
        await vault.bridge.shutdown()
    }

    /// A task dropped on the grid becomes a block as long as the planner
    /// says the task is: its estimate.
    @Test
    func aDroppedTaskTakesItsEstimate() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        var task = draft("Draft the proposal")
        task.estimatedDurationS = 90 * 60
        let created = try await vault.bridge.submit(.createTask(draft: task))

        await model.dropTask(created.entity, atMs: hour(model, 14))

        #expect(try start(model, "Draft the proposal") == hour(model, 14))
        #expect(try end(model, "Draft the proposal") == hour(model, 15.5))
        await vault.bridge.shutdown()
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
}

/// The one-in-flight rule for previews, without a vault.
struct PlanPreviewPumpTests {
    @Test
    func theFirstPositionIsSentAtOnce() {
        var pump = PlanPreviewPump<Int>()
        #expect(pump.request(1) == 1)
        #expect(pump.inFlight)
    }

    /// While one call runs, newer positions queue, each replacing the last;
    /// the result that comes back is then stale and the newest is sent.
    @Test
    func aNewerPositionReplacesTheQueuedOneAndSupersedesTheResult() {
        var pump = PlanPreviewPump<Int>()
        _ = pump.request(1)
        #expect(pump.request(2) == nil)
        #expect(pump.request(3) == nil)

        let first = pump.finished()
        #expect(!first.current, "the result for 1 is not drawn")
        #expect(first.next == 3, "2 was replaced by 3")

        let second = pump.finished()
        #expect(second.current)
        #expect(second.next == nil)
        #expect(!pump.inFlight)
    }

    /// The gesture ended: what was queued is dropped.
    @Test
    func resetDropsTheQueuedPosition() {
        var pump = PlanPreviewPump<Int>()
        _ = pump.request(1)
        _ = pump.request(2)
        pump.reset()
        let done = pump.finished()
        #expect(done.next == nil)
        #expect(!pump.inFlight)
    }
}

/// How the planning keys add up, without a vault.
struct PlanNudgeTests {
    @Test
    func movesAndDaysAddUpAndAResizeStartsAgain() {
        var nudge = PlanNudge(block: "blk_1")
        nudge.press(.later)
        nudge.press(.later)
        nudge.press(.nextDay)
        #expect(nudge.mode == .move)
        #expect(nudge.steps == 2)
        #expect(nudge.days == 1)

        nudge.press(.shorter)
        #expect(nudge.mode == .resizeEnd)
        #expect(nudge.steps == -1)
        #expect(nudge.days == 0)
    }
}

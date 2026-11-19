import Foundation
import Testing

@testable import Sunrise

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

    private func ghosts(_ model: CalendarModel) -> [PlanGhost] {
        model.planner.ghosts(
            fromMs: model.dayStartMs(offset: 0),
            toMs: model.dayStartMs(offset: 1),
            timeZone: model.timeZone
        )
    }

    private func drag(
        _ model: CalendarModel, _ title: String, _ mode: BlockDragMode, to hours: Double
    ) async throws {
        let row = try row(model, title)
        await model.planner.previewDrag(row, mode: mode, toMs: hour(model, hours), timeZone: model.timeZone)
    }

    private func drop(
        _ model: CalendarModel, _ title: String, _ mode: BlockDragMode, at hours: Double
    ) async throws {
        let row = try row(model, title)
        await model.planner.dropDrag(row, mode: mode, toMs: hour(model, hours), timeZone: model.timeZone)
    }

    private func press(_ model: CalendarModel, _ key: PlanKey, on title: String) async throws -> Bool {
        let row = try row(model, title)
        let (snap, zone) = (model.snapMinutes, model.timeZone)
        return await model.planner.press(key, on: row, snapMinutes: snap, timeZone: zone)
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
        let standup = try row(model, "Standup").block.id
        let focus = try row(model, "Focus").block.id
        #expect(!(try row(model, "Focus").isFixed), "a block Sunrise made is flexible")

        try await drag(model, "Standup", .move, to: 10)

        let plan = try #require(model.planner.plan)
        #expect(plan.committable)
        #expect(Set(plan.moves.map(\.item)) == [standup, focus])
        #expect(ghosts(model).count == 2)
        let rippled = try #require(ghosts(model).first { $0.item == focus })
        #expect(rippled.role == .rippled)
        #expect(rippled.before == GridSpan(startMs: hour(model, 10), endMs: hour(model, 11)))
        let ghostOfFocus = try #require(rippled.after)
        #expect(model.planner.summary?.hasPrefix(L10n.Calendar.planMoves(count: 2)) == true)
        #expect(try start(model, "Standup") == hour(model, 9), "a preview writes nothing")

        try await drop(model, "Standup", .move, at: 10)

        #expect(model.planner.plan == nil)
        #expect(try start(model, "Standup") == hour(model, 10))
        #expect(try start(model, "Focus") == ghostOfFocus.startMs, "Focus landed where its ghost was")
        #expect(try end(model, "Focus") == ghostOfFocus.endMs)

        let undo = UndoModel(bridge: vault.bridge)
        await undo.refresh()
        let label = L10n.Calendar.undoPlanMove(title: "Standup", count: 2)
        #expect(undo.undoTitle == L10n.Undo.undoNamed(action: label))
        await undo.undo()
        await model.refresh()
        #expect(try start(model, "Standup") == hour(model, 9))
        #expect(try start(model, "Focus") == hour(model, 10))
        #expect(try end(model, "Focus") == hour(model, 11))
        #expect(undo.lastAction == L10n.Undo.undid(action: label))
        #expect(!undo.canRedo, "a planner commit has no redo")
        await vault.bridge.shutdown()
    }

    /// A resize is planned too: the end moves, the start stays, and the time
    /// keeps its kind.
    @Test
    func aResizeMovesTheEndAndKeepsTheStartAndTheKind() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        await model.createBlock(
            fromMs: hour(model, 9), toMs: hour(model, 10), title: "Reading", kind: .floating
        )

        try await drop(model, "Reading", .resizeEnd, at: 11.5)

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
        let undo = UndoModel(bridge: vault.bridge)
        await undo.refresh()
        let before = undo.undoTitle

        try await drag(model, "Standup", .move, to: 10)
        #expect(model.planner.plan != nil)
        try await drop(model, "Standup", .move, at: 9)

        #expect(model.planner.plan == nil)
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

        try await drag(model, "Standup", .move, to: 10)
        #expect(!ghosts(model).isEmpty)
        model.planner.cancel()

        #expect(model.planner.plan == nil)
        #expect(ghosts(model).isEmpty)
        #expect(model.planner.summary == nil)
        await model.refresh()
        #expect(try start(model, "Focus") == hour(model, 10))
        await vault.bridge.shutdown()
    }
}

// MARK: - Drops that do not go as previewed, and the keyboard

extension PlannedDragTests {
    /// Write new times to `title` straight through the bridge, as another
    /// window or a synced device would: off this model's undo stack, and
    /// under the planner's feet.
    private func moveBehindThePlanner(
        _ vault: borrowing TestVault, _ model: CalendarModel, _ title: String, from: Double, to: Double
    ) async throws {
        let row = try row(model, title)
        let zone = model.timeZone
        var edit = BlockEdit()
        edit.startsAt = CalendarModel.sameKind(as: row.block.startsAt, ms: hour(model, from), timeZone: zone)
        edit.endsAt = CalendarModel.sameKind(as: row.block.endsAt, ms: hour(model, to), timeZone: zone)
        _ = try await vault.bridge.submit(.updateBlock(id: row.block.id, edit: edit))
    }

    /// **A stale drop whose answer did not change is committed.** A block
    /// written between the preview and the drop makes the diff on screen
    /// stale; the drop previews once more, gets the same moves, and commits
    /// them without asking.
    @Test
    func aStaleDropWhosePlanIsTheSameIsCommittedAnyway() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        await twoBlocks(model)
        await model.createBlock(
            fromMs: hour(model, 15), toMs: hour(model, 16), title: "Reading", kind: .zoned
        )

        try await drag(model, "Standup", .move, to: 10)
        let plan = try #require(model.planner.plan)
        let ghostOfFocus = try #require(ghosts(model).first { $0.role == .rippled }?.after)
        try await moveBehindThePlanner(vault, model, "Reading", from: 16, to: 17)
        await #expect(throws: BindingError.self, "the write made the diff on screen stale") {
            _ = try await vault.bridge.planCommit(plan.id, label: "stale")
        }

        try await drop(model, "Standup", .move, at: 10)

        #expect(model.planner.plan == nil)
        #expect(model.planner.note == nil)
        #expect(!model.planner.awaitingDecision)
        #expect(try start(model, "Standup") == hour(model, 10))
        #expect(try start(model, "Focus") == ghostOfFocus.startMs)
        #expect(try start(model, "Reading") == hour(model, 16), "the write behind the planner stands")
        await vault.bridge.shutdown()
    }

    /// **A stale drop whose answer changed waits, and the button commits
    /// it.** Focus moved out of the way between the preview and the drop, so
    /// the new plan moves Standup alone. Nothing is written until the person
    /// commits the plan now on screen.
    @Test
    func aStaleDropWhosePlanChangedWaitsUntilItIsCommitted() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        await twoBlocks(model)
        let standup = try row(model, "Standup").block.id

        try await drag(model, "Standup", .move, to: 10)
        #expect(model.planner.plan?.moves.count == 2)
        try await moveBehindThePlanner(vault, model, "Focus", from: 13, to: 14)

        try await drop(model, "Standup", .move, at: 10)

        let waiting = try #require(model.planner.plan, "the new plan is left on screen")
        #expect(waiting.moves.map(\.item) == [standup])
        #expect(model.planner.note == L10n.Calendar.notePlanChanged)
        #expect(model.planner.awaitingDecision)
        #expect(try start(model, "Standup") == hour(model, 9), "nothing written before the answer")
        #expect(try start(model, "Focus") == hour(model, 13))

        await model.planner.commitPlan()

        #expect(model.planner.plan == nil)
        #expect(model.planner.note == nil)
        #expect(!model.planner.awaitingDecision)
        #expect(try start(model, "Standup") == hour(model, 10))
        #expect(try start(model, "Focus") == hour(model, 13))
        await vault.bridge.shutdown()
    }

    /// The same changed plan, cancelled: the ghosts and the note go, and
    /// nothing is written.
    @Test
    func aStaleDropWhosePlanChangedCanBeCancelled() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        await twoBlocks(model)

        try await drag(model, "Standup", .move, to: 10)
        try await moveBehindThePlanner(vault, model, "Focus", from: 13, to: 14)
        try await drop(model, "Standup", .move, at: 10)
        #expect(model.planner.awaitingDecision)

        model.planner.cancel()

        #expect(model.planner.plan == nil)
        #expect(ghosts(model).isEmpty)
        #expect(model.planner.note == nil)
        #expect(!model.planner.awaitingDecision)
        await model.refresh()
        #expect(try start(model, "Standup") == hour(model, 9))
        await vault.bridge.shutdown()
    }

    /// A drop the planner refuses writes nothing and says why: a resize
    /// that ends the block before it starts.
    @Test
    func aRefusedDropWritesNothingAndSaysSo() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        await twoBlocks(model)

        try await drop(model, "Standup", .resizeEnd, at: 8.5)

        #expect(model.planner.plan == nil)
        #expect(model.planner.note == L10n.Calendar.notePlanRefused)
        #expect(!model.planner.awaitingDecision, "a refusal waits for nothing")
        await model.refresh()
        #expect(try end(model, "Standup") == hour(model, 10))
        await vault.bridge.shutdown()
    }

    /// **Undo after a later edit restores what it can.** Focus was moved
    /// again after the commit, so undo puts Standup back, leaves Focus
    /// where the later write put it, and says how much it restored.
    @Test
    func undoAfterALaterEditRestoresTheRestAndSaysHowMuch() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        await twoBlocks(model)
        try await drop(model, "Standup", .move, at: 10)
        #expect(try start(model, "Standup") == hour(model, 10))
        try await moveBehindThePlanner(vault, model, "Focus", from: 13, to: 14)

        let undo = UndoModel(bridge: vault.bridge)
        await undo.refresh()
        await undo.undo()
        await model.refresh()

        let label = L10n.Calendar.undoPlanMove(title: "Standup", count: 2)
        let partial = L10n.Undo.restoredSome(restored: 2, total: 4)
        #expect(undo.lastAction == "\(L10n.Undo.undid(action: label)). \(partial)")
        #expect(try start(model, "Standup") == hour(model, 9))
        #expect(try start(model, "Focus") == hour(model, 13), "the later write is kept")
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

        #expect(try await press(model, .later, on: "Standup"))
        #expect(try await press(model, .later, on: "Standup"))
        let subject = try #require(model.planner.plan?.moves.first)
        guard case let .slot(start, _) = subject.after else {
            Issue.record("the nudge did not preview a slot")
            return
        }
        #expect(timeValueMs(value: start, tz: model.timeZone) == hour(model, 9.5))
        #expect(try self.start(model, "Standup") == hour(model, 9), "nothing written before Return")

        #expect(try await press(model, .commit, on: "Standup"))
        #expect(try self.start(model, "Standup") == hour(model, 9.5))
        #expect(model.planner.nudge == nil)
        await vault.bridge.shutdown()
    }

    /// The resize keys move the end, and Esc drops the nudge.
    @Test
    func theResizeKeysMoveTheEndAndEscapeDropsThem() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        model.snapMinutes = 30
        await model.createBlock(fromMs: hour(model, 9), toMs: hour(model, 10), title: "Standup", kind: .zoned)

        #expect(try await press(model, .longer, on: "Standup"))
        let subject = try #require(model.planner.plan?.moves.first)
        guard case let .slot(start, end) = subject.after else {
            Issue.record("the nudge did not preview a slot")
            return
        }
        #expect(timeValueMs(value: start, tz: model.timeZone) == hour(model, 9))
        #expect(timeValueMs(value: end, tz: model.timeZone) == hour(model, 10.5))

        #expect(try await press(model, .cancel, on: "Standup"))
        #expect(model.planner.plan == nil)
        #expect(!(try await press(model, .commit, on: "Standup")), "nothing left to commit")
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

        await model.planner.dropTask(created.entity, atMs: hour(model, 14), timeZone: model.timeZone)

        #expect(try start(model, "Draft the proposal") == hour(model, 14))
        #expect(try end(model, "Draft the proposal") == hour(model, 15.5))
        await vault.bridge.shutdown()
    }

    /// One event of a repeating block is not planned; its drag writes the
    /// span the grid computes, keeping the event's length.
    @Test
    func aRepeatingEventKeepsItsLengthWhenMovedAndCannotEndBeforeItStarts() throws {
        let row = BlockGridRow(
            block: BlockItem(
                id: "blk_1",
                createdAt: 0,
                updatedAt: 0,
                streamId: inboxStreamId(),
                startsAt: .instant(at: 9 * 3_600_000),
                endsAt: .instant(at: 10 * 3_600_000),
                title: nil,
                titleTrackTask: false,
                tasks: [],
                deleted: false
            ),
            title: nil,
            taskTitles: [],
            occurrenceKey: nil
        )
        let moved = CalendarPlanner.repeatingSpan(of: row, mode: .move, toMs: 11 * 3_600_000, timeZone: "UTC")
        #expect(moved == GridSpan(startMs: 11 * 3_600_000, endMs: 12 * 3_600_000))
        let squashed = CalendarPlanner.repeatingSpan(
            of: row, mode: .resizeEnd, toMs: 9 * 3_600_000, timeZone: "UTC"
        )
        #expect(squashed == nil)
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

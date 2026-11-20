import Foundation
import Testing

@testable import Sunrise

/// Upcoming's civil-day arithmetic and headings, with no vault.
struct UpcomingLayoutTests {
    @Test
    func addingDaysCrossesMonthsYearsAndLeapDays() {
        #expect(UpcomingLayout.adding(1, to: "2026-11-30") == "2026-12-01")
        #expect(UpcomingLayout.adding(1, to: "2026-12-31") == "2027-01-01")
        #expect(UpcomingLayout.adding(-1, to: "2028-03-01") == "2028-02-29")
        #expect(UpcomingLayout.adding(0, to: "2026-03-08") == "2026-03-08")
    }

    /// Civil days, not 24-hour steps: the day after a DST change in any zone
    /// is the next date, which arithmetic on instants would not promise.
    @Test
    func addingIsCivilAcrossADaylightSavingChange() {
        #expect(UpcomingLayout.adding(1, to: "2026-03-08") == "2026-03-09")
        #expect(UpcomingLayout.adding(1, to: "2026-11-01") == "2026-11-02")
    }

    @Test
    func todayAndTomorrowAreNamedAndLaterDaysAreDated() {
        #expect(UpcomingLayout.heading(for: "2026-11-19", today: "2026-11-19") == L10n.Upcoming.today)
        #expect(UpcomingLayout.heading(for: "2026-11-20", today: "2026-11-19") == L10n.Upcoming.tomorrow)
        let later = UpcomingLayout.heading(for: "2026-11-23", today: "2026-11-19")
        #expect(later != L10n.Upcoming.today && later != L10n.Upcoming.tomorrow)
        #expect(later.contains("23"), "a later day is headed by its date: \(later)")
    }

    @Test
    func aDropOnADayIsABulkDeferAndOnTheLaneAnUnschedule() {
        let tasks: [EntityRef] = ["tsk_1"]
        let onADay = UpcomingPlanner.intent(moving: tasks, to: .day("2026-11-21"))
        guard case let .bulkDefer(moved, day, place) = onADay else {
            Issue.record("a day is a BulkDefer")
            return
        }
        #expect(moved == tasks && day == "2026-11-21" && !place)
        guard case let .unschedule(items, keep) = UpcomingPlanner.intent(moving: tasks, to: .noDate) else {
            Issue.record("the no-date lane is an Unschedule")
            return
        }
        #expect(items == tasks && keep == .nothing)
    }

    @Test
    func upcomingIsAListTheSidebarOffersAndCaptureDoesNotLandIn() {
        #expect(Destination.fixed.contains(.list(.upcomingDefault)))
        #expect(
            !TaskListKind.upcomingDefault.acceptsCapture,
            "a line dated past the span, or undated past the lane's cap, would be written and not drawn"
        )
        #expect(TaskListKind.upcomingDefault.emptyHint.isEmpty)
        #expect(!TaskListKind.upcomingDefault.acceptsReordering)
        guard case let .upcoming(_, span, contexts) = TaskListKind.upcomingDefault.query(nowMs: 7) else {
            Issue.record("Upcoming reads Query::Upcoming")
            return
        }
        #expect(span == nil, "the sidebar's Upcoming reads the device's span")
        #expect(contexts.isEmpty)
        #expect(Keymap.action(for: KeyChord("3", [.command]), scope: .application) == .upcoming)
    }
}

/// Upcoming against a real vault: the days and the no-date lane as the core
/// composes them, and rescheduling through the planner (#351).
@MainActor
struct UpcomingModelTests {
    private func model(_ vault: borrowing TestVault) async -> TaskListModel {
        let model = TaskListModel(bridge: vault.bridge, kind: .upcomingDefault)
        await model.refresh()
        return model
    }

    private func task(_ title: String, on date: CivilDate?) -> TaskDraftIn {
        TaskDraftIn(
            title: title,
            body: nil,
            streamId: nil,
            contexts: [],
            priority: nil,
            energy: nil,
            estimatedDurationS: nil,
            scheduledAt: date.map { .allDay(date: $0) },
            dueAt: nil,
            schedulingConstraints: [],
            assignee: nil,
            reminderLeadS: nil
        )
    }

    /// The day a task is drawn under, or `nil` when it is not on a day.
    private func day(of title: String, in model: TaskListModel) -> CivilDate? {
        model.upcoming.first { $0.date != nil && $0.rows.contains { $0.task.title == title } }?.date
    }

    private func id(of title: String, in model: TaskListModel) throws -> EntityRef {
        try #require(model.tasks.first { $0.title == title }).id
    }

    @Test
    func theDaysComeFromTheCoreAndAnUndatedTaskIsInTheNoDateLane() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        #expect(model.showsNothing)
        #expect(model.emptyDescription(hinted: false) == L10n.Upcoming.empty(span: "7"))
        let tomorrow = UpcomingLayout.adding(1, to: model.upcomingToday)

        _ = try await vault.bridge.submit(.createTask(draft: task("Report", on: tomorrow)))
        _ = try await vault.bridge.submit(.createTask(draft: task("Someday", on: nil)))
        await model.refresh()

        #expect(model.upcoming.filter { $0.date != nil }.count == 7)
        #expect(model.upcoming.first?.date == model.upcomingToday)
        #expect(day(of: "Report", in: model) == tomorrow)
        let lane = try #require(model.upcoming.first { $0.date == nil })
        #expect(lane.rows.map(\.task.title) == ["Someday"])
        #expect(lane.rows.first?.mark == .undated)
        #expect(!model.showsNothing)
        #expect(model.upcomingNotice == nil, "a day holds something")
        await vault.bridge.shutdown()
    }

    @Test
    func onlyUndatedTasksSayTheDaysAreEmpty() async throws {
        let vault = try await TestVault()
        _ = try await vault.bridge.submit(.createTask(draft: task("Someday", on: nil)))
        let model = await model(vault)
        #expect(model.upcomingNotice == L10n.Upcoming.onlyUndated(span: "7"))
        await vault.bridge.shutdown()
    }

    /// **Drag a task to another day.** The hover previews the core's plan
    /// and writes nothing; the drop commits it; one undo puts it back.
    @Test
    func aTaskDraggedToAnotherDayMovesThereAndOneUndoReturnsIt() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        let tomorrow = UpcomingLayout.adding(1, to: model.upcomingToday)
        let later = UpcomingLayout.adding(3, to: model.upcomingToday)
        _ = try await vault.bridge.submit(.createTask(draft: task("Report", on: tomorrow)))
        await model.refresh()
        let report = try id(of: "Report", in: model)

        model.upcomingPlanner.beginDrag([report])
        await model.upcomingPlanner.hover(.day(later))
        let plan = try #require(model.upcomingPlanner.plan)
        #expect(plan.committable)
        #expect(plan.moves.map(\.item) == [report])
        #expect(model.upcomingPlanner.target == .day(later))
        #expect(model.upcomingPlanner.summary?.hasPrefix(L10n.Calendar.planMoves(count: 1)) == true)
        await model.refresh()
        #expect(day(of: "Report", in: model) == tomorrow, "a preview writes nothing")

        model.upcomingPlanner.willDrop()
        await model.upcomingPlanner.hover(nil, leaving: .day(later))
        #expect(model.upcomingPlanner.plan != nil, "the hover ending with a drop leaves its plan")
        await model.reschedule([report], to: .day(later))

        #expect(model.upcomingPlanner.plan == nil)
        #expect(day(of: "Report", in: model) == later)

        let undo = UndoModel(bridge: vault.bridge)
        await undo.refresh()
        let label = L10n.Upcoming.undoMove(title: "Report")
        #expect(undo.undoTitle == L10n.Undo.undoNamed(action: label))
        await undo.undo()
        await model.refresh()
        #expect(day(of: "Report", in: model) == tomorrow)
        await vault.bridge.shutdown()
    }

    @Test
    func leavingASectionDropsItsPlanAndWritesNothing() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        let tomorrow = UpcomingLayout.adding(1, to: model.upcomingToday)
        let later = UpcomingLayout.adding(2, to: model.upcomingToday)
        _ = try await vault.bridge.submit(.createTask(draft: task("Report", on: tomorrow)))
        await model.refresh()

        model.upcomingPlanner.beginDrag([try id(of: "Report", in: model)])
        await model.upcomingPlanner.hover(.day(later))
        #expect(model.upcomingPlanner.plan != nil)
        await model.upcomingPlanner.hover(nil, leaving: .day(tomorrow))
        #expect(model.upcomingPlanner.plan != nil, "leaving a section whose plan is not drawn is old news")
        await model.upcomingPlanner.hover(nil, leaving: .day(later))
        #expect(model.upcomingPlanner.plan == nil)
        await model.refresh()
        #expect(day(of: "Report", in: model) == tomorrow)
        await vault.bridge.shutdown()
    }

    /// Into the no-date lane: the plan is cleared, and the task is undated.
    @Test
    func aTaskDroppedInTheNoDateLaneLosesItsPlan() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        let tomorrow = UpcomingLayout.adding(1, to: model.upcomingToday)
        _ = try await vault.bridge.submit(.createTask(draft: task("Report", on: tomorrow)))
        await model.refresh()

        await model.reschedule([try id(of: "Report", in: model)], to: .noDate)

        #expect(day(of: "Report", in: model) == nil)
        let lane = try #require(model.upcoming.first { $0.date == nil })
        #expect(lane.rows.map(\.task.title) == ["Report"])
        await vault.bridge.shutdown()
    }

    /// **The keyboard equivalent.** ⌥→ twice previews two days on, Return
    /// commits it.
    @Test
    func theKeyboardMovesTheSelectionADayAtATimeAndReturnCommits() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        let tomorrow = UpcomingLayout.adding(1, to: model.upcomingToday)
        _ = try await vault.bridge.submit(.createTask(draft: task("Report", on: tomorrow)))
        await model.refresh()
        let report = try id(of: "Report", in: model)

        await model.moveUpcoming([report], days: 1)
        await model.moveUpcoming([report], days: 1)
        let twoOn = UpcomingLayout.adding(2, to: tomorrow)
        #expect(model.upcomingPlanner.target == .day(twoOn))
        await model.refresh()
        #expect(day(of: "Report", in: model) == tomorrow, "a keyboard move previews; it writes nothing")

        await model.commitUpcomingPlan()
        #expect(day(of: "Report", in: model) == twoOn)
        #expect(model.upcomingPlanner.nudge == nil)
        await vault.bridge.shutdown()
    }

    @Test
    func escapeDropsAKeyboardMove() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        _ = try await vault.bridge.submit(.createTask(draft: task("Someday", on: nil)))
        await model.refresh()
        let someday = try id(of: "Someday", in: model)

        await model.moveUpcoming([someday], days: 1)
        let today = model.upcomingToday
        #expect(model.upcomingPlanner.target == .day(today), "an undated task's first ⌥→ is today")
        model.upcomingPlanner.cancel()
        #expect(model.upcomingPlanner.plan == nil)
        await model.refresh()
        #expect(day(of: "Someday", in: model) == nil)
        await vault.bridge.shutdown()
    }

    /// The span chips write the device's preference, which the next
    /// Upcoming reads.
    @Test
    func theSpanIsTheDevicesPreference() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        #expect(model.upcomingSpan == 7)

        await model.setUpcomingSpan(14)
        #expect(model.upcomingSpan == 14)
        #expect(model.upcoming.filter { $0.date != nil }.count == 14)

        let again = await self.model(vault)
        #expect(again.upcomingSpan == 14)
        #expect(again.kind == .upcomingDefault, "the sidebar's entry stays the one selected")
        await vault.bridge.shutdown()
    }
}

/// A drop whose diff went stale, or that the planner refuses, as
/// `PlannedDragTests` drives the calendar's: the same contract over tasks and
/// a day.
extension UpcomingModelTests {
    /// Report, planned for tomorrow and previewed onto three days on.
    private func hovered(_ vault: borrowing TestVault) async throws -> (TaskListModel, EntityRef) {
        let model = await model(vault)
        let tomorrow = UpcomingLayout.adding(1, to: model.upcomingToday)
        _ = try await vault.bridge.submit(.createTask(draft: task("Report", on: tomorrow)))
        await model.refresh()
        let report = try id(of: "Report", in: model)
        model.upcomingPlanner.beginDrag([report])
        await model.upcomingPlanner.hover(.day(UpcomingLayout.adding(3, to: model.upcomingToday)))
        #expect(model.upcomingPlanner.plan != nil)
        return (model, report)
    }

    /// Report moved to two days on behind the planner, which changes the
    /// diff a drop on three days on would commit.
    private func moveBehindThePlanner(
        _ vault: borrowing TestVault, _ report: EntityRef, to date: CivilDate
    ) async throws {
        var edit = TaskEdit()
        edit.setScheduledAt = .allDay(date: date)
        _ = try await vault.bridge.submit(.updateTask(id: report, edit: edit))
    }

    /// **A stale drop whose answer did not change is committed.** Another
    /// task planned on the target day between the hover and the drop makes
    /// the diff on screen stale; the drop previews once more, gets the same
    /// move, and commits it without asking.
    @Test
    func aStaleDropWhosePlanIsTheSameIsCommittedAnyway() async throws {
        let vault = try await TestVault()
        let (model, report) = try await hovered(vault)
        let later = UpcomingLayout.adding(3, to: model.upcomingToday)
        let plan = try #require(model.upcomingPlanner.plan)
        _ = try await vault.bridge.submit(.createTask(draft: task("Errand", on: later)))
        await #expect(throws: BindingError.self, "the write made the diff on screen stale") {
            _ = try await vault.bridge.planCommit(plan.id, label: "stale")
        }

        await model.reschedule([report], to: .day(later))

        #expect(model.upcomingPlanner.plan == nil)
        #expect(model.upcomingPlanner.note == nil)
        #expect(!model.upcomingPlanner.awaitingDecision)
        #expect(day(of: "Report", in: model) == later)
        #expect(day(of: "Errand", in: model) == later, "the write behind the planner stands")
        await vault.bridge.shutdown()
    }

    /// **A stale drop whose answer changed waits, and Move commits it.**
    /// Report was moved behind the planner, so the new plan moves it from a
    /// different day. Nothing is written until the plan now on screen is
    /// committed.
    @Test
    func aStaleDropWhosePlanChangedWaitsUntilItIsCommitted() async throws {
        let vault = try await TestVault()
        let (model, report) = try await hovered(vault)
        let twoOn = UpcomingLayout.adding(2, to: model.upcomingToday)
        let later = UpcomingLayout.adding(3, to: model.upcomingToday)
        try await moveBehindThePlanner(vault, report, to: twoOn)

        await model.reschedule([report], to: .day(later))

        let waiting = try #require(model.upcomingPlanner.plan, "the new plan is left on screen")
        #expect(waiting.moves.map(\.item) == [report])
        #expect(model.upcomingPlanner.note == L10n.Calendar.notePlanChanged)
        #expect(model.upcomingPlanner.awaitingDecision)
        #expect(model.upcomingPlanner.target == .day(later))
        #expect(day(of: "Report", in: model) == twoOn, "nothing written before the answer")

        await model.commitUpcomingPlan()

        #expect(model.upcomingPlanner.plan == nil)
        #expect(model.upcomingPlanner.note == nil)
        #expect(!model.upcomingPlanner.awaitingDecision)
        #expect(day(of: "Report", in: model) == later)
        await vault.bridge.shutdown()
    }

    /// The same changed plan, cancelled: the plan and its note go, and
    /// nothing is written.
    @Test
    func aStaleDropWhosePlanChangedCanBeCancelled() async throws {
        let vault = try await TestVault()
        let (model, report) = try await hovered(vault)
        let twoOn = UpcomingLayout.adding(2, to: model.upcomingToday)
        try await moveBehindThePlanner(vault, report, to: twoOn)
        await model.reschedule([report], to: .day(UpcomingLayout.adding(3, to: model.upcomingToday)))
        #expect(model.upcomingPlanner.awaitingDecision)

        model.upcomingPlanner.cancel()

        #expect(model.upcomingPlanner.plan == nil)
        #expect(model.upcomingPlanner.target == nil)
        #expect(model.upcomingPlanner.note == nil)
        #expect(!model.upcomingPlanner.awaitingDecision)
        await model.refresh()
        #expect(day(of: "Report", in: model) == twoOn)
        await vault.bridge.shutdown()
    }

    /// A drop the planner refuses writes nothing and says why: a task held
    /// to tomorrow's weekday by a hard constraint, dropped on another day.
    @Test
    func aRefusedDropWritesNothingAndSaysSo() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        let tomorrow = UpcomingLayout.adding(1, to: model.upcomingToday)
        var draft = task("Report", on: tomorrow)
        draft.schedulingConstraints = [Constraint(
            timeOfDay: nil, daysOfWeek: [weekday(of: tomorrow)], dateRange: nil,
            severity: .hard, atPlace: [], extra: nil
        )]
        _ = try await vault.bridge.submit(.createTask(draft: draft))
        await model.refresh()

        let later = UpcomingLayout.adding(3, to: model.upcomingToday)
        await model.reschedule([try id(of: "Report", in: model)], to: .day(later))

        #expect(model.upcomingPlanner.plan == nil)
        #expect(model.upcomingPlanner.note == L10n.Upcoming.notePlanRefused)
        #expect(!model.upcomingPlanner.awaitingDecision, "a refusal waits for nothing")
        await model.refresh()
        #expect(day(of: "Report", in: model) == tomorrow)
        await vault.bridge.shutdown()
    }

    /// ⌥→ counts from a task's plan, not from the deadline's day it is first
    /// drawn under: planned after its hard deadline, it still moves later.
    @Test
    func theKeyboardCountsFromThePlanNotTheDeadline() async throws {
        let vault = try await TestVault()
        let model = await model(vault)
        let due = UpcomingLayout.adding(2, to: model.upcomingToday)
        let planned = UpcomingLayout.adding(4, to: model.upcomingToday)
        var draft = task("Report", on: planned)
        draft.dueAt = .allDay(date: due)
        _ = try await vault.bridge.submit(.createTask(draft: draft))
        await model.refresh()
        let report = try id(of: "Report", in: model)
        #expect(day(of: "Report", in: model) == due, "first drawn under its deadline's day")

        await model.moveUpcoming([report], days: 1)

        #expect(model.upcomingPlanner.target == .day(UpcomingLayout.adding(1, to: planned)))
        model.upcomingPlanner.cancel()
        await vault.bridge.shutdown()
    }

    /// The weekday `date` falls on.
    private func weekday(of date: CivilDate) -> Weekday {
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(identifier: "UTC") ?? .current
        let parts = date.split(separator: "-").compactMap { Int($0) }
        let noon = calendar.date(
            from: DateComponents(year: parts[0], month: parts[1], day: parts[2], hour: 12)
        )
        // `Calendar` counts from Sunday, 1.
        let order: [Weekday] = [.su, .mo, .tu, .we, .th, .fr, .sa]
        return order[calendar.component(.weekday, from: noon ?? Date()) - 1]
    }
}

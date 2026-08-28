import Foundation
import Testing

@testable import Sunrise

/// Helpers shared by the task tests.
enum Fixture {
    static let zone = "America/New_York"

    /// 2026-03-10T17:00 in New York.
    static let now: UInt64 = {
        var components = DateComponents()
        components.year = 2026
        components.month = 3
        components.day = 10
        components.hour = 17
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = TimeZone(identifier: zone) ?? .gmt
        let date = calendar.date(from: components) ?? Date(timeIntervalSince1970: 0)
        return UInt64(date.timeIntervalSince1970 * 1000)
    }()

    static func task(
        id: String = "tsk_01ARZ3NDEKTSV4RRFFQ69G5FAV",
        title: String = "Renew passport",
        state: TaskState = .todo,
        streamId: String = "str_01ARZ3NDEKTSV4RRFFQ69G5FAV",
        contexts: [String] = [],
        priority: UInt8? = nil,
        energy: Energy? = nil,
        estimateS: UInt64? = nil,
        scheduledAt: TimeValue? = nil,
        dueAt: TimeValue? = nil,
        constraints: [Constraint] = [],
        blockedBy: [String] = [],
        deferred: Int64 = 0
    ) -> TaskItem {
        TaskItem(
            id: id,
            createdAt: 0,
            updatedAt: 0,
            title: title,
            body: nil,
            streamId: streamId,
            contexts: contexts,
            state: state,
            priority: priority,
            energy: energy,
            estimatedDurationS: estimateS,
            scheduledAt: scheduledAt,
            dueAt: dueAt,
            schedulingConstraints: constraints,
            completedAt: nil,
            deferredCount: deferred,
            blocks: [],
            blockedBy: blockedBy,
            assignee: nil,
            routineId: nil,
            routineOccurrence: nil,
            reminderLeadS: nil,
            archived: false,
            deleted: false
        )
    }
}

struct TaskFacetsTests {
    private let names = NameBook(
        streams: ["str_01ARZ3NDEKTSV4RRFFQ69G5FAV": "travel"],
        contexts: ["ctx_01ARZ3NDEKTSV4RRFFQ69G5FAV": "errands"]
    )

    /// Every word on a row comes from the seam, so the app and `sunrise-cli`
    /// cannot disagree about what a task says.
    @Test
    func aRowWordsItselfFromTheSharedVocabulary() {
        let facets = TaskFacets(
            task: Fixture.task(
                contexts: ["ctx_01ARZ3NDEKTSV4RRFFQ69G5FAV"],
                priority: 1,
                energy: .med,
                estimateS: 5400,
                dueAt: .allDay(date: "2026-03-11")
            ),
            nowMs: Fixture.now,
            timeZone: Fixture.zone,
            names: names
        )

        #expect(facets.estimate == "1h30")
        #expect(facets.energy == "med")
        #expect(facets.due == DayLabel(RelativeDay(text: "tomorrow", isPast: false)))
        #expect(facets.streamName == "travel")
        #expect(facets.contextNames == ["errands"])
        #expect(facets.priority == 1)
    }

    /// The case a hand-written `dueAt < now` gets wrong: a deadline earlier
    /// today is not late.
    @Test
    func aDeadlineEarlierTodayIsNotMarkedLate() {
        let facets = TaskFacets(
            task: Fixture.task(dueAt: .zoned(civil: "2026-03-10T09:00:00", tz: Fixture.zone)),
            nowMs: Fixture.now,
            timeZone: Fixture.zone,
            names: names
        )
        #expect(facets.due?.text == "today")
        #expect(facets.due?.isPast == false)
    }

    @Test
    func aDeadlineLastWeekIsMarkedLate() {
        let facets = TaskFacets(
            task: Fixture.task(dueAt: .allDay(date: "2026-03-03")),
            nowMs: Fixture.now,
            timeZone: Fixture.zone,
            names: names
        )
        #expect(facets.due?.text == "-7d")
        #expect(facets.due?.isPast == true)
    }

    /// A context deleted between the list read and the name read must not
    /// leave a raw id on screen.
    @Test
    func anUnknownIdIsOmittedRatherThanShownRaw() {
        let facets = TaskFacets(
            task: Fixture.task(contexts: ["ctx_01BX5ZZKBKACTAV9WEVGEMMVRZ"]),
            nowMs: Fixture.now,
            timeZone: Fixture.zone,
            names: names
        )
        #expect(facets.contextNames.isEmpty)
    }

    @Test
    func aTaskWithNoFacetsCarriesNone() {
        let facets = TaskFacets(
            task: Fixture.task(),
            nowMs: Fixture.now,
            timeZone: Fixture.zone,
            names: names
        )
        #expect(facets.energy == nil)
        #expect(facets.estimate == nil)
        #expect(facets.due == nil)
        #expect(facets.constraints == nil)
        #expect(!facets.isBlocked)
    }

    @Test
    func blockersAndConstraintsAreVisible() {
        let constraint = Constraint(
            timeOfDay: nil,
            daysOfWeek: [],
            dateRange: nil,
            severity: .hard
        )
        let facets = TaskFacets(
            task: Fixture.task(constraints: [constraint], blockedBy: ["tsk_01BX5ZZKBKACTAV9WEVGEMMVRZ"]),
            nowMs: Fixture.now,
            timeZone: Fixture.zone,
            names: names
        )
        #expect(facets.constraints == "1 constraint (1 hard)")
        #expect(facets.isBlocked)
    }
}

struct TaskGroupingTests {
    /// The spec's sections, in the spec's order, and nothing empty.
    @Test
    func todayIsGroupedTheWayTheSpecComposesIt() {
        let scheduled = Fixture.task(
            id: "tsk_01ARZ3NDEKTSV4RRFFQ69G5FAV",
            scheduledAt: .zoned(civil: "2026-03-10T15:00:00", tz: Fixture.zone)
        )
        let dueToday = Fixture.task(
            id: "tsk_01BX5ZZKBKACTAV9WEVGEMMVRZ",
            dueAt: .allDay(date: "2026-03-10")
        )
        let overdue = Fixture.task(
            id: "tsk_01C0000000000000000000000",
            dueAt: .allDay(date: "2026-03-01")
        )

        let groups = TaskGrouping.today(
            tasks: [overdue, dueToday, scheduled],
            nowMs: Fixture.now,
            timeZone: Fixture.zone
        )

        #expect(groups.map(\.section) == [.scheduled, .due, .overdue])
        #expect(groups[0].tasks.map(\.id) == [scheduled.id])
        #expect(groups[1].tasks.map(\.id) == [dueToday.id])
        #expect(groups[2].tasks.map(\.id) == [overdue.id])
    }

    /// The core sorted by `COALESCE(scheduled_at, due_at)`; re-sorting here
    /// would replace its ordering with this file's opinion.
    @Test
    func theCoresOrderSurvivesInsideASection() {
        let first = Fixture.task(
            id: "tsk_01C0000000000000000000001",
            scheduledAt: .zoned(civil: "2026-03-10T09:00:00", tz: Fixture.zone)
        )
        let second = Fixture.task(
            id: "tsk_01C0000000000000000000002",
            scheduledAt: .zoned(civil: "2026-03-10T15:00:00", tz: Fixture.zone)
        )

        let groups = TaskGrouping.today(
            tasks: [first, second],
            nowMs: Fixture.now,
            timeZone: Fixture.zone
        )
        #expect(groups.count == 1)
        #expect(groups[0].tasks.map(\.id) == [first.id, second.id])
    }

    @Test
    func anEmptySectionIsNotAnEmptyHeading() {
        let groups = TaskGrouping.today(
            tasks: [Fixture.task(dueAt: .allDay(date: "2026-03-10"))],
            nowMs: Fixture.now,
            timeZone: Fixture.zone
        )
        #expect(groups.map(\.section) == [.due])
    }

    @Test
    func overdueStartsFoldedAndNothingElseDoes() {
        #expect(TodaySection.overdue.startsFolded)
        #expect(!TodaySection.scheduled.startsFolded)
        #expect(!TodaySection.due.startsFolded)
    }
}

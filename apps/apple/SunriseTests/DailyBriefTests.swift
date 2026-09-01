import Foundation
import Testing

@testable import Sunrise

/// The morning summary, against a real vault.
///
/// The two briefs are the views issue #9 asks a notification to open into, and
/// `docs/08-features/notifications.md` is explicit that "the client's job is
/// to render them, not to recompute them" — so what is asserted here is the
/// rendering decisions, and nothing that would amount to a second
/// implementation of the report.
@MainActor
struct MorningSummaryModelTests {
    private func draft(_ title: String, scheduledAt: TimeValue? = nil) -> TaskDraftIn {
        TaskDraftIn(
            title: title,
            body: nil,
            streamId: nil,
            contexts: [],
            priority: nil,
            energy: nil,
            estimatedDurationS: nil,
            scheduledAt: scheduledAt,
            dueAt: nil,
            schedulingConstraints: [],
            assignee: nil,
            reminderLeadS: nil
        )
    }

    @Test
    func anEmptyVaultStillShowsEverySection() async throws {
        let vault = try await TestVault()
        let model = MorningSummaryModel(bridge: vault.bridge)
        await model.refresh()

        #expect(model.sections.count == 4)
        #expect(model.sections.allSatisfy { $0.tasks.isEmpty })
        #expect(
            model.sections.allSatisfy { !$0.emptyMessage.isEmpty },
            "an empty section is the answer somebody opened the view for"
        )
        #expect(model.errorMessage == nil)
        await vault.bridge.shutdown()
    }

    /// The reason `MorningReport` carries `today_start` at all — the seam's
    /// own comment says it is there "so a client can split `completed` …
    /// without a second query", and this is that client. Anything finished
    /// since midnight is today's, not yesterday's.
    @Test
    func aTaskCompletedThisMorningIsSeparatedFromYesterdays() async throws {
        let vault = try await TestVault()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(draft("Ship the thing"))
        await list.complete(try #require(list.tasks.first))

        let model = MorningSummaryModel(bridge: vault.bridge)
        await model.refresh()

        let today = try #require(model.sections.first { $0.id == "today" })
        let yesterday = try #require(model.sections.first { $0.id == "yesterday" })
        #expect(today.tasks.map(\.title) == ["Ship the thing"])
        #expect(yesterday.tasks.isEmpty)
        await vault.bridge.shutdown()
    }

    @Test
    func anOpenInboxTaskIsSomethingToTriage() async throws {
        let vault = try await TestVault()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(draft("Decide about the car"))

        let model = MorningSummaryModel(bridge: vault.bridge)
        await model.refresh()

        let triage = try #require(model.sections.first { $0.id == "triage" })
        #expect(triage.tasks.map(\.title) == ["Decide about the car"])
        #expect(model.headline.contains("1 to triage"))
        await vault.bridge.shutdown()
    }

    /// Completing from the brief writes through the core and the view catches
    /// up, exactly as a list row does.
    @Test
    func completingFromTheBriefMovesTheTaskBetweenSections() async throws {
        let vault = try await TestVault()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(draft("Book the ferry"))
        let task = try #require(list.tasks.first)

        let model = MorningSummaryModel(bridge: vault.bridge)
        await model.refresh()
        await model.complete(task)

        let triage = try #require(model.sections.first { $0.id == "triage" })
        let today = try #require(model.sections.first { $0.id == "today" })
        #expect(triage.tasks.isEmpty)
        #expect(today.tasks.map(\.title) == ["Book the ferry"])
        await vault.bridge.shutdown()
    }

    /// The headline is the one line somebody reads before the sections. It has
    /// to say something even when the day is empty.
    @Test
    func theHeadlineAlwaysSaysSomething() async throws {
        let vault = try await TestVault()
        let model = MorningSummaryModel(bridge: vault.bridge)
        #expect(!model.headline.isEmpty, "before the first read, too")
        await model.refresh()
        #expect(model.headline.contains("0 completed since yesterday"))
        await vault.bridge.shutdown()
    }
}

/// The end-of-day plan, against a real vault.
@MainActor
struct EndOfDayPlanModelTests {
    private func draft(_ title: String, scheduledAt: TimeValue? = nil) -> TaskDraftIn {
        TaskDraftIn(
            title: title,
            body: nil,
            streamId: nil,
            contexts: [],
            priority: nil,
            energy: nil,
            estimatedDurationS: nil,
            scheduledAt: scheduledAt,
            dueAt: nil,
            schedulingConstraints: [],
            assignee: nil,
            reminderLeadS: nil
        )
    }

    @Test
    func anEmptyVaultStillShowsEverySection() async throws {
        let vault = try await TestVault()
        let model = EndOfDayPlanModel(bridge: vault.bridge)
        await model.refresh()

        #expect(model.sections.map(\.id) == ["open", "week", "backlog"])
        #expect(model.errorMessage == nil)
        await vault.bridge.shutdown()
    }

    /// A task with neither a date nor a deadline is the backlog the evening
    /// plans from.
    @Test
    func anUndatedTaskIsTheBacklogToPlanFrom() async throws {
        let vault = try await TestVault()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(draft("Someday: learn the piano"))

        let model = EndOfDayPlanModel(bridge: vault.bridge)
        await model.refresh()

        let backlog = try #require(model.sections.first { $0.id == "backlog" })
        #expect(backlog.tasks.map(\.title) == ["Someday: learn the piano"])
        #expect(model.headline.contains("1 unscheduled"))
        await vault.bridge.shutdown()
    }

    /// The evening's whole verb: something scheduled for earlier today is
    /// still open, and one press moves it.
    @Test
    func todaysLeftoversMoveToTomorrowInOnePress() async throws {
        let vault = try await TestVault()
        let now = await vault.bridge.nowMs()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(
            draft("Reply to Sam", scheduledAt: .instant(at: Int64(now) - 3_600_000))
        )
        await list.create(
            draft("Read the contract", scheduledAt: .instant(at: Int64(now) - 7_200_000))
        )

        let model = EndOfDayPlanModel(bridge: vault.bridge)
        await model.refresh()
        let open = try #require(model.sections.first { $0.id == "open" })
        #expect(open.count == 2)

        await model.moveEverythingToTomorrow()

        await list.refresh()
        #expect(
            list.tasks.allSatisfy { $0.deferredCount == 1 },
            "each move is a deferral, which is how a slipping task becomes visible"
        )
        let stillOpen = try #require(model.sections.first { $0.id == "open" })
        #expect(stillOpen.tasks.isEmpty, "tomorrow is not today")
        await vault.bridge.shutdown()
    }

    /// Nothing to move is a no-op, not an error banner.
    @Test
    func movingNothingToTomorrowDoesNothing() async throws {
        let vault = try await TestVault()
        let model = EndOfDayPlanModel(bridge: vault.bridge)
        await model.refresh()
        await model.moveEverythingToTomorrow()

        #expect(model.errorMessage == nil)
        await vault.bridge.shutdown()
    }

    /// The snooze spans the two briefs offer are the domain's, in one place,
    /// so a menu here and a notification button cannot disagree.
    @Test
    func theOfferedSpansAreTheDomainsOwn() {
        #expect(SnoozeSpan.offered == [.oneHour, .tomorrow, .nextWeek])
        #expect(SnoozeSpan.tomorrow.buttonTitle == "Snooze Until Tomorrow")
    }
}

/// Both briefs are reachable without ever allowing a notification.
struct BriefDestinationTests {
    @Test
    func bothBriefsAreInTheSidebar() {
        #expect(Destination.fixed.contains(.morning))
        #expect(Destination.fixed.contains(.evening))
    }

    /// Every fixed destination needs a label and a symbol, or the sidebar draws
    /// a blank row.
    @Test
    func everyFixedDestinationIsRenderable() {
        for destination in Destination.fixed {
            #expect(!destination.title.isEmpty)
            #expect(!destination.symbol.isEmpty)
        }
    }
}

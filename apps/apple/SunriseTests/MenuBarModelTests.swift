import Foundation
import Testing

@testable import Sunrise

/// The menu bar snapshot, against a real vault.
///
/// The counts matter more than they look: this is the surface a user leaves
/// open all day, so it is the one most likely to be showing yesterday's
/// numbers, and the one where a disagreement with the main window is most
/// visible.
@MainActor
struct MenuBarModelTests {
    private func draft(_ title: String, dueAt: TimeValue? = nil) -> TaskDraftIn {
        TaskDraftIn(
            title: title,
            body: nil,
            streamId: nil,
            contexts: [],
            priority: nil,
            energy: nil,
            estimatedDurationS: nil,
            scheduledAt: nil,
            dueAt: dueAt,
            schedulingConstraints: [],
            assignee: nil,
            reminderLeadS: nil
        )
    }

    @Test
    func anEmptyVaultHasNothingOutstanding() async throws {
        let vault = try await TestVault()
        let model = MenuBarModel(bridge: vault.bridge)
        await model.refresh()

        #expect(model.snapshot.isEmpty)
        #expect(model.snapshot.badge.isEmpty, "an empty badge draws no number at all")
        await vault.bridge.shutdown()
    }

    @Test
    func theInboxCountIsOpenTasksOnly() async throws {
        let vault = try await TestVault()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(draft("Triage this"))
        await list.create(draft("And this"))
        await list.complete(try #require(list.tasks.first))

        let model = MenuBarModel(bridge: vault.bridge)
        await model.refresh()

        #expect(model.snapshot.inbox == 1, "a completed task is not still to triage")
        await vault.bridge.shutdown()
    }

    /// The overdue boundary is `due_at < start_of_today_local`, not
    /// `due_at < now`. A menu bar that counted it itself would disagree with
    /// the list behind it every evening — so it asks `today_section`.
    @Test
    func aDeadlineEarlierTodayCountsAsDueRatherThanOverdue() async throws {
        let vault = try await TestVault()
        let now = await vault.bridge.nowMs()
        var calendar = Calendar(identifier: .gregorian)
        calendar.timeZone = .current
        let startOfDay = calendar.startOfDay(for: Date(timeIntervalSince1970: Double(now) / 1000))
        let earlierToday = Int64(startOfDay.timeIntervalSince1970 * 1000) + 60_000
        let yesterday = Int64(startOfDay.timeIntervalSince1970 * 1000) - 60_000

        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(draft("Due earlier today", dueAt: .instant(at: earlierToday)))
        await list.create(draft("Was due yesterday", dueAt: .instant(at: yesterday)))

        let model = MenuBarModel(bridge: vault.bridge)
        await model.refresh()

        #expect(model.snapshot.due == 1)
        #expect(model.snapshot.overdue == 1)
        #expect(model.snapshot.badge == "2")
        await vault.bridge.shutdown()
    }

    /// A completed task leaves the outstanding counts and appears in the one
    /// that says the day went somewhere.
    @Test
    func completingATaskMovesItOutOfTheBadge() async throws {
        let vault = try await TestVault()
        let now = await vault.bridge.nowMs()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(draft("Ship it", dueAt: .instant(at: Int64(now))))

        let model = MenuBarModel(bridge: vault.bridge)
        await model.refresh()
        #expect(model.snapshot.badge == "1")

        await list.complete(try #require(list.tasks.first))
        await model.refresh()

        #expect(model.snapshot.badge.isEmpty)
        #expect(model.snapshot.doneToday == 1)
        await vault.bridge.shutdown()
    }

    /// Opening the menu refreshes it. Sitting closed for an hour and then
    /// showing hour-old numbers is the failure this exists to prevent.
    @Test
    func becomingVisibleTriggersARefresh() async throws {
        let vault = try await TestVault()
        let model = MenuBarModel(bridge: vault.bridge)
        #expect(model.lastRefreshMs == 0)

        model.isVisible = true
        // The `didSet` starts a task; give it the one hop it needs.
        try await _Concurrency.Task.sleep(for: .milliseconds(50))

        #expect(model.lastRefreshMs > 0)
        await vault.bridge.shutdown()
    }

    /// The spec's numbers, asserted rather than assumed: a poll interval that
    /// drifted to five minutes would still pass every other test here.
    @Test
    func theRefreshPolicyIsTheOneTheSpecStates() {
        #expect(MenuBarModel.pollInterval == .seconds(60))
        #expect(MenuBarModel.changeDebounce == .milliseconds(500))
    }
}

#if os(macOS)
/// The hotkey's failure states, which are the ones a headless run can reach.
///
/// The whole suite is macOS-only, rather than guarded line by line: a global
/// hotkey is a registration with the window server, and iOS has neither. The
/// capture surfaces that stand in for it there — the sheet, the Control Center
/// control, the widget, the App Shortcut — cannot fail to register, so there
/// is no equivalent failure state to name.
struct HotkeyStatusTests {
    /// The ordinary failure — another app got ⌘⇧N first — is not the same as
    /// the app being broken, and the two need different sentences.
    @Test
    func aTakenShortcutIsDistinguishedFromAFailure() {
        #expect(HotkeyCenter.classify(HotkeyCenter.alreadyTaken) == .taken)
        #expect(HotkeyCenter.classify(-50) == .unavailable(-50))
    }

    /// Every state says what happened *and* that capture still works. A status
    /// line that only said "unavailable" would leave someone believing the
    /// feature was gone.
    @Test
    func everyInactiveStateStillPointsAtTheMenuBar() {
        for status in [HotkeyStatus.taken, .unavailable(-50)] {
            #expect(!status.isActive)
            #expect(status.explanation.contains("menu bar"))
        }
        #expect(HotkeyStatus.active.isActive)
        #expect(HotkeyStatus.active.explanation.contains("⌘⇧N"))
    }
}
#endif

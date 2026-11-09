import Foundation
import Testing

@testable import Sunrise

/// The rest of the catalog through the scheduler: the kinds the core plans
/// beside the reminders (`docs/08-features/notifications.md` §Catalog), against
/// a real vault and the fake notification service ``ReminderSchedulerTests``
/// uses, for the same reasons.
@MainActor
struct CatalogSchedulerTests {
    private func scratchDefaults(_ name: String) -> UserDefaults {
        guard let defaults = UserDefaults(suiteName: name) else {
            fatalError("could not open a scratch defaults suite")
        }
        return defaults
    }

    private func discard(_ name: String) {
        UserDefaults.standard.removePersistentDomain(forName: name)
    }

    private func scheduledTask(_ title: String, at ms: Int64) -> TaskDraftIn {
        TaskDraftIn(
            title: title,
            body: nil,
            streamId: nil,
            contexts: [],
            priority: nil,
            energy: nil,
            estimatedDurationS: nil,
            scheduledAt: .instant(at: ms),
            dueAt: nil,
            schedulingConstraints: [],
            assignee: nil,
            reminderLeadS: nil
        )
    }

    private func makeScheduler(
        _ vault: borrowing TestVault,
        _ name: String,
        center: FakeNotificationCenter,
        route: @escaping @MainActor (DeepLink) -> Void = { _ in }
    ) -> (ReminderScheduler, NotificationPreferences) {
        let preferences = NotificationPreferences(defaults: scratchDefaults(name))
        let scheduler = ReminderScheduler(
            bridge: vault.bridge,
            preferences: preferences,
            center: center,
            route: route
        )
        return (scheduler, preferences)
    }

    /// The briefs reach the OS as the core planned them — a fresh vault with
    /// no day schedule plans both, at 08:00 and 20:00 — each under its own
    /// category, so its buttons are the catalog's. And a second pass with
    /// nothing changed hands nothing over: the revision stored with each one
    /// is read back and matches.
    @Test
    func theBriefsAreScheduledWithTheirCategoriesAndOnlyOnce() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let center = FakeNotificationCenter()
        let (scheduler, _) = makeScheduler(vault, name, center: center)

        await scheduler.start()
        let kinds = scheduler.catalogScheduled.map(\.category)
        #expect(kinds.contains(.catalog(.morningBrief)))
        #expect(kinds.contains(.catalog(.eveningPlan)))
        #expect(scheduler.catalogPolicy != nil, "Settings has the switches to show")
        let first = await center.scheduled.count

        await scheduler.reconcile()
        await scheduler.reconcile()

        let after = await center.scheduled.count
        #expect(after == first, "an unchanged catalog is not handed over again")
        await vault.bridge.shutdown()
    }

    /// A switch in Settings reaches the vault, and the next plan honours it:
    /// the morning brief is withdrawn, the evening plan stays.
    @Test
    func turningAKindOffWithdrawsItAndLeavesTheOthers() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let center = FakeNotificationCenter()
        let (scheduler, _) = makeScheduler(vault, name, center: center)
        await scheduler.start()
        let morning = try #require(
            scheduler.catalogScheduled.first { $0.category == .catalog(.morningBrief) }
        )

        await scheduler.setKind(.morningBrief, enabled: false)

        let policy = try #require(scheduler.catalogPolicy)
        let pending = await center.pending
        #expect(!NotificationCatalog.isOn(.morningBrief, in: policy))
        #expect(!pending.contains(morning.identifier))
        #expect(scheduler.catalogScheduled.contains { $0.category == .catalog(.eveningPlan) })
        #expect(scheduler.errorMessage == nil)
        await vault.bridge.shutdown()
    }

    /// The task-reminder switch reaches the reminders this client plans.
    @Test
    func turningTaskRemindersOffWithdrawsThem() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let center = FakeNotificationCenter()
        let (scheduler, _) = makeScheduler(vault, name, center: center)
        let now = await vault.bridge.nowMs()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(scheduledTask("Pay rent", at: Int64(now) + 600_000))
        await scheduler.start()
        #expect(scheduler.scheduled.count == 1)

        await scheduler.setKind(.taskReminder, enabled: false)

        let pending = await center.pendingReminders
        #expect(scheduler.scheduled.isEmpty)
        #expect(pending.isEmpty)
        await vault.bridge.shutdown()
    }

    /// A device that is not the primary one holds none of the primary
    /// device's kinds: the briefs go with the reminders.
    @Test
    func aDeviceThatIsNotPrimaryHoldsNoBriefs() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let center = FakeNotificationCenter()
        let (scheduler, preferences) = makeScheduler(vault, name, center: center)
        await scheduler.start()
        #expect(!scheduler.catalogScheduled.isEmpty)

        preferences.isPrimaryDevice = false
        await scheduler.reconcile()

        let pending = await center.pending
        #expect(scheduler.catalogScheduled.isEmpty)
        #expect(pending.isEmpty)
        await vault.bridge.shutdown()
    }

    /// A snooze on a brief posts it again later, under an identifier no
    /// re-plan cancels, and the original's plan is left as it was.
    @Test
    func snoozingABriefPostsItAgainUnderItsOwnIdentifier() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let center = FakeNotificationCenter()
        let (scheduler, _) = makeScheduler(vault, name, center: center)
        await scheduler.start()
        let brief = try #require(scheduler.catalogScheduled.first)
        let now = await vault.bridge.nowMs()

        await scheduler.handle(.catalog(.snoozeFifteenMinutes, brief))
        await scheduler.reconcile()

        let identifier = ReminderPlan.snoozedIdentifier(brief.identifier)
        let handed = await center.scheduled
        let snoozed = try #require(handed.last(where: { $0.identifier == identifier }))
        let pending = await center.pending
        #expect(snoozed.fireAtMs >= Int64(now) + 15 * 60_000)
        #expect(snoozed.title == brief.title)
        #expect(pending.contains(identifier), "the re-plan leaves the snooze alone")
        await vault.bridge.shutdown()
    }

    /// A button that opens a screen is routed: Open Triage to the triage
    /// section, Plan My Day to the notification's own link; Keep does nothing.
    @Test
    func aCatalogButtonThatOpensAScreenIsRouted() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let box = LinkBox()
        let (scheduler, _) = makeScheduler(vault, name, center: FakeNotificationCenter()) {
            box.value = $0
        }
        let brief = PlannedNotification(
            identifier: "sunrise.notify.morning_brief",
            title: "Good morning",
            body: "",
            category: .catalog(.morningBrief),
            fireAtMs: 0,
            entity: "",
            link: .morningSummary
        )

        await scheduler.handle(.catalog(.openTriage, brief))
        #expect(box.value == .triage)
        await scheduler.handle(.catalog(.planMyDay, brief))
        #expect(box.value == .morningSummary)
        box.value = nil
        await scheduler.handle(.catalog(.keep, brief))
        #expect(box.value == nil)
        await vault.bridge.shutdown()
    }

    /// The significant-time-change observer. A clock change says nothing to
    /// the change feed, so the scheduler listens for it and re-plans: a task
    /// written behind its back is scheduled once the day rolls over, with no
    /// follow loop running.
    @Test
    func aClockChangeRePlans() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let center = FakeNotificationCenter()
        let (scheduler, _) = makeScheduler(vault, name, center: center)
        await scheduler.start()
        defer { scheduler.stopObservingTimeChanges() }
        #expect(scheduler.scheduled.isEmpty)

        let now = await vault.bridge.nowMs()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(scheduledTask("Feed the cat", at: Int64(now) + 600_000))
        NotificationCenter.default.post(name: .NSCalendarDayChanged, object: nil)

        let deadline = ContinuousClock.now.advanced(by: .seconds(5))
        while scheduler.scheduled.isEmpty, ContinuousClock.now < deadline {
            try? await Task.sleep(for: .milliseconds(20))
        }
        #expect(scheduler.scheduled.first?.title == "Feed the cat")
        await vault.bridge.shutdown()
    }
}

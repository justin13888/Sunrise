import Foundation
import Testing

@testable import Sunrise

/// A notification service that answers, and remembers what it was asked.
///
/// An actor, because the scheduler holds it as a `Sendable` collaborator and
/// the real one is a process-wide singleton. It models the two behaviours that
/// matter: `pending` grows when something is scheduled and shrinks when it is
/// cancelled, so a second reconcile sees the first one's work — which is the
/// whole thing under test.
actor FakeNotificationCenter: NotificationCenterClient {
    private var status: NotificationAuthorization
    /// What the system will answer when asked.
    private let answersRequestWith: NotificationAuthorization
    private(set) var pending: [String] = []
    private(set) var scheduled: [PlannedNotification] = []
    private(set) var cancelled: [String] = []
    private(set) var categoryRegistrations = 0
    private(set) var requestCount = 0
    private(set) var hasDelegate = false

    init(
        status: NotificationAuthorization = .authorized,
        answersRequestWith: NotificationAuthorization? = nil
    ) {
        self.status = status
        self.answersRequestWith = answersRequestWith ?? status
    }

    func authorization() async -> NotificationAuthorization { status }

    func requestAuthorization() async -> NotificationAuthorization {
        requestCount += 1
        status = answersRequestWith
        return status
    }

    func registerCategories() async { categoryRegistrations += 1 }

    func install(delegate: ReminderResponder) async { hasDelegate = true }

    func pendingIdentifiers() async -> [String] { pending }

    func schedule(_ notification: PlannedNotification) async throws {
        scheduled.append(notification)
        if !pending.contains(notification.identifier) {
            pending.append(notification.identifier)
        }
    }

    func cancel(identifiers: [String]) async {
        cancelled.append(contentsOf: identifiers)
        pending.removeAll { identifiers.contains($0) }
    }
}

/// Somewhere for the routing closure to leave what it was handed.
@MainActor
final class LinkBox {
    var value: DeepLink?
}

/// The scheduler, against a real vault and a fake notification service.
///
/// The vault is real because the reminders come from `Query::ReminderIntents`
/// and an FFI seam only fails at the boundary; the service is fake because the
/// pending list belongs to the *machine*, and two tests sharing one would see
/// each other's alerts — and asking it for authorization would put a real
/// prompt on the developer's screen.
@MainActor
struct ReminderSchedulerTests {
    /// A scratch defaults suite, so a test cannot rewrite the developer's own
    /// quiet hours.
    private func scratchDefaults(_ name: String) -> UserDefaults {
        guard let defaults = UserDefaults(suiteName: name) else {
            fatalError("could not open a scratch defaults suite")
        }
        return defaults
    }

    private func discard(_ name: String) {
        UserDefaults.standard.removePersistentDomain(forName: name)
    }

    /// A task scheduled inside the horizon, which is what produces an intent.
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

    /// The base case, and the one every other test here is a variation of.
    @Test
    func aScheduledTaskReachesTheNotificationService() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let center = FakeNotificationCenter()
        let scheduler = ReminderScheduler(
            bridge: vault.bridge,
            preferences: NotificationPreferences(defaults: scratchDefaults(name)),
            center: center
        ) { _ in }

        let now = await vault.bridge.nowMs()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(scheduledTask("Call the dentist", at: Int64(now) + 600_000))

        await scheduler.start()

        #expect(scheduler.scheduled.count == 1)
        #expect(scheduler.scheduled.first?.title == "Call the dentist")
        let hasDelegate = await center.hasDelegate
        #expect(hasDelegate, "the buttons need somewhere to report to")
        let registrations = await center.categoryRegistrations
        #expect(registrations == 1)
        await vault.bridge.shutdown()
    }

    /// **The one that matters.** A sync burst re-plans repeatedly, and the OS
    /// must end up holding one alert per intent rather than one per pass —
    /// `UNUserNotificationCenter.add` on an identifier it already has
    /// *replaces* the request, which re-arms the trigger rather than doing
    /// nothing.
    @Test
    func reschedulingTenTimesAddsNothingAfterTheFirst() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let center = FakeNotificationCenter()
        let scheduler = ReminderScheduler(
            bridge: vault.bridge,
            preferences: NotificationPreferences(defaults: scratchDefaults(name)),
            center: center
        ) { _ in }

        let now = await vault.bridge.nowMs()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(scheduledTask("Renew passport", at: Int64(now) + 900_000))

        await scheduler.start()
        for _ in 0..<9 { await scheduler.reconcile() }

        let added = await center.scheduled.count
        let cancelled = await center.cancelled
        #expect(added == 1, "one alert, not ten")
        #expect(cancelled.isEmpty)
        #expect(scheduler.lastPlan.isEmpty, "an unchanged schedule is not rewritten")
        #expect(scheduler.lastPlan.kept.count == 1)
        await vault.bridge.shutdown()
    }

    /// Refusing the permission must cost nothing but the reminders.
    @Test
    func nothingIsScheduledWithoutPermission() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let center = FakeNotificationCenter(status: .denied)
        let scheduler = ReminderScheduler(
            bridge: vault.bridge,
            preferences: NotificationPreferences(defaults: scratchDefaults(name)),
            center: center
        ) { _ in }

        let now = await vault.bridge.nowMs()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(scheduledTask("Water the plants", at: Int64(now) + 600_000))

        await scheduler.start()

        let added = await center.scheduled
        let asked = await center.requestCount
        #expect(scheduler.scheduled.isEmpty)
        #expect(added.isEmpty)
        #expect(scheduler.errorMessage == nil, "a refusal is not a failure")
        #expect(asked == 0, "an answered question is not asked again")
        await vault.bridge.shutdown()
    }

    /// The prompt happens once, on the first unlock, and only then.
    @Test
    func anUnaskedDeviceAsksExactlyOnce() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let center = FakeNotificationCenter(
            status: .notDetermined,
            answersRequestWith: .authorized
        )
        let scheduler = ReminderScheduler(
            bridge: vault.bridge,
            preferences: NotificationPreferences(defaults: scratchDefaults(name)),
            center: center
        ) { _ in }

        await scheduler.start()
        await scheduler.reconcile()

        let asked = await center.requestCount
        #expect(asked == 1)
        #expect(scheduler.authorization == .authorized)
        await vault.bridge.shutdown()
    }

    /// A device told not to remind must not be asked for permission either:
    /// asking for something the app has been told not to use is how people
    /// learn to press Don't Allow without reading.
    @Test
    func aDeviceWithRemindersOffIsNeverPrompted() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let preferences = NotificationPreferences(defaults: scratchDefaults(name))
        preferences.isEnabled = false
        let center = FakeNotificationCenter(
            status: .notDetermined,
            answersRequestWith: .authorized
        )
        let scheduler = ReminderScheduler(
            bridge: vault.bridge,
            preferences: preferences,
            center: center
        ) { _ in }

        await scheduler.start()

        let asked = await center.requestCount
        #expect(asked == 0)
        await vault.bridge.shutdown()
    }

    /// `docs/08-features/notifications.md` §Multi-device dedup. The core
    /// returns an empty list before it reads a row — and the Mac that *was*
    /// primary has to go quiet, not merely stop adding.
    @Test
    func demotingTheDeviceWithdrawsWhatItAlreadyScheduled() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let preferences = NotificationPreferences(defaults: scratchDefaults(name))
        let center = FakeNotificationCenter()
        let scheduler = ReminderScheduler(
            bridge: vault.bridge,
            preferences: preferences,
            center: center
        ) { _ in }

        let now = await vault.bridge.nowMs()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(scheduledTask("Standup", at: Int64(now) + 600_000))

        await scheduler.start()
        let armed = scheduler.scheduled.map(\.identifier)
        #expect(armed.count == 1)

        preferences.isPrimaryDevice = false
        await scheduler.reconcile()

        let cancelled = await center.cancelled
        let pending = await center.pending
        #expect(scheduler.scheduled.isEmpty)
        #expect(cancelled == armed)
        #expect(pending.isEmpty)
        await vault.bridge.shutdown()
    }

    /// The master switch reaches the same end state from the other direction.
    @Test
    func turningRemindersOffWithdrawsThemToo() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let preferences = NotificationPreferences(defaults: scratchDefaults(name))
        let center = FakeNotificationCenter()
        let scheduler = ReminderScheduler(
            bridge: vault.bridge,
            preferences: preferences,
            center: center
        ) { _ in }

        let now = await vault.bridge.nowMs()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(scheduledTask("Post the letter", at: Int64(now) + 600_000))

        await scheduler.start()
        let armed = await center.pending.count
        #expect(armed == 1)

        preferences.isEnabled = false
        await scheduler.reconcile()

        let pending = await center.pending
        #expect(pending.isEmpty)
        await vault.bridge.shutdown()
    }

    /// The "Mark Done" button, all the way to the vault — and the schedule
    /// re-derived afterwards, so a completed task's alert does not still fire.
    @Test
    func theDoneButtonCompletesTheTaskAndWithdrawsItsAlert() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let center = FakeNotificationCenter()
        let scheduler = ReminderScheduler(
            bridge: vault.bridge,
            preferences: NotificationPreferences(defaults: scratchDefaults(name)),
            center: center
        ) { _ in }

        let now = await vault.bridge.nowMs()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(scheduledTask("File the tax return", at: Int64(now) + 600_000))
        await scheduler.start()
        let task = try #require(list.tasks.first)

        await scheduler.handle(.act(task.id, .complete))

        await list.refresh()
        let pending = await center.pending
        #expect(list.tasks.first?.state == .done)
        #expect(scheduler.scheduled.isEmpty)
        #expect(pending.isEmpty)
        await vault.bridge.shutdown()
    }

    /// "Snooze until tomorrow" is the domain's civil arithmetic, not a
    /// millisecond addition here — the reason commit `6be6044` exists. The
    /// assertion is deliberately loose about *where* it lands: the exact
    /// instant is the seam's answer, and re-deriving it here would be
    /// re-implementing the thing under test.
    @Test
    func snoozingMovesTheTaskADayOutAndWithdrawsTheOldAlert() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let center = FakeNotificationCenter()
        let scheduler = ReminderScheduler(
            bridge: vault.bridge,
            preferences: NotificationPreferences(defaults: scratchDefaults(name)),
            center: center
        ) { _ in }

        let now = await vault.bridge.nowMs()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(scheduledTask("Ring the bank", at: Int64(now) + 600_000))
        await scheduler.start()
        let armed = try #require(scheduler.scheduled.first)
        let task = try #require(list.tasks.first)

        await scheduler.handle(.act(task.id, .snooze(.tomorrow)))

        await list.refresh()
        let moved = try #require(list.tasks.first?.scheduledAt)
        let movedMs = timeValueMs(value: moved, tz: TimeZone.current.identifier)
        let cancelled = await center.cancelled
        #expect(list.tasks.first?.deferredCount == 1)
        #expect(
            movedMs > Int64(now) + 20 * 60 * 60 * 1000,
            "tomorrow is a date, not an hour later"
        )
        #expect(cancelled.contains(armed.identifier), "the alert it moved off is withdrawn")
        await vault.bridge.shutdown()
    }

    /// A tapped body is routed, not executed.
    @Test
    func tappingTheBodyIsHandedToTheRouterAndNothingElse() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let box = LinkBox()
        let scheduler = ReminderScheduler(
            bridge: vault.bridge,
            preferences: NotificationPreferences(defaults: scratchDefaults(name)),
            center: FakeNotificationCenter()
        ) { link in box.value = link }

        await scheduler.handle(.open(.morningSummary))
        #expect(box.value == .morningSummary)

        await scheduler.handle(.dismissed)
        #expect(box.value == .morningSummary, "a dismissal changes nothing")
        await vault.bridge.shutdown()
    }

    /// **The workaround this replaced.**
    ///
    /// The scheduler used to re-derive the schedule on a five-minute timer,
    /// because `CoreBridge.changes()` allowed exactly one subscriber and a
    /// scheduler that followed it would have stolen the feed from whatever
    /// screen the user was looking at. A reminder created on another device
    /// therefore took up to five minutes to reach this Mac. The bridge fans
    /// out now, so the scheduler follows — and the write below arrives without
    /// anything asking for it.
    @Test
    func aWriteFromAnywhereReachesTheScheduleWithoutAPoll() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let center = FakeNotificationCenter()
        let scheduler = ReminderScheduler(
            bridge: vault.bridge,
            preferences: NotificationPreferences(defaults: scratchDefaults(name)),
            center: center
        ) { _ in }

        await scheduler.start()
        #expect(scheduler.scheduled.isEmpty, "nothing is scheduled yet")

        let following = Task { await scheduler.follow(debounce: .milliseconds(10)) }
        defer { following.cancel() }
        // `follow` has to reach `changes()` before the write, or there is no
        // notification to receive. Two actor hops; this is generous.
        try? await Task.sleep(for: .milliseconds(100))

        // The write a sync would have delivered — made behind the scheduler's
        // back, exactly as another device's op arrives.
        let now = await vault.bridge.nowMs()
        let list = TaskListModel(bridge: vault.bridge, kind: .inbox)
        await list.create(scheduledTask("Collect the parcel", at: Int64(now) + 600_000))

        let deadline = ContinuousClock.now.advanced(by: .seconds(5))
        while scheduler.scheduled.isEmpty, ContinuousClock.now < deadline {
            try? await Task.sleep(for: .milliseconds(20))
        }
        #expect(
            scheduler.scheduled.first?.title == "Collect the parcel",
            "the scheduler is not following the change feed"
        )
        await vault.bridge.shutdown()
    }

    /// The one timer left, asserted rather than assumed. It covers the
    /// authorization status and nothing else: that is the single fact the
    /// change feed cannot report, because revoking permission in System
    /// Settings is not a vault write.
    @Test
    func onlyTheAuthorizationStatusIsStillOnATimer() {
        #expect(ReminderScheduler.authorizationRefreshInterval == .seconds(300))
        #expect(ReminderScheduler.changeDebounce == .milliseconds(500))
    }
}

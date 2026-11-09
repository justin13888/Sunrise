import Foundation
import Testing

@testable import Sunrise

/// What the scheduler does with what the core hands back beyond the briefs:
/// wind-down's add, replace and cancel, an alert the core raises and the
/// ledger that remembers it, and where a kind's switch is written. Against a
/// real vault and ``FakeNotificationCenter``, as ``CatalogSchedulerTests``.
///
/// The zones are named rather than read from the machine: Tokyo and
/// Vancouver are never the same offset, so moving between them always moves
/// wind-down and always raises the zone notice.
@MainActor
struct CatalogDeliveryTests {
    private let tokyo = "Asia/Tokyo"
    private let vancouver = "America/Vancouver"

    private func makeScheduler(
        _ vault: borrowing TestVault,
        _ name: String,
        center: FakeNotificationCenter
    ) -> (ReminderScheduler, NotificationPreferences) {
        guard let defaults = UserDefaults(suiteName: name) else {
            fatalError("could not open a scratch defaults suite")
        }
        let preferences = NotificationPreferences(defaults: defaults)
        let scheduler = ReminderScheduler(
            bridge: vault.bridge,
            preferences: preferences,
            center: center,
            route: { _ in }
        )
        return (scheduler, preferences)
    }

    private func discard(_ name: String) {
        UserDefaults.standard.removePersistentDomain(forName: name)
    }

    /// The scheduler's alerts that are the zone notice.
    private func zoneNotices(_ center: FakeNotificationCenter) async -> [PlannedNotification] {
        await center.scheduled.filter { $0.category == .catalog(.timezoneChanged) }
    }

    /// A vault with the zone notice on, planned once in Tokyo, so the ledger
    /// has seen that zone and the next move raises the notice.
    private func plannedInTokyo(
        _ vault: borrowing TestVault,
        _ name: String,
        center: FakeNotificationCenter
    ) async throws -> (ReminderScheduler, NotificationPreferences) {
        _ = try await vault.bridge.reportTimeZone(tokyo)
        let (scheduler, preferences) = makeScheduler(vault, name, center: center)
        await scheduler.start()
        scheduler.stopObservingTimeChanges()
        await scheduler.setKind(.timezoneChanged, enabled: true)
        return (scheduler, preferences)
    }

    /// Wind-down through the scheduler: a sleep time schedules it under the
    /// core's identifier, a zone move replaces it with a new revision, and
    /// switching it off withdraws exactly that identifier.
    @Test
    func windDownIsAddedReplacedAndWithdrawn() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let center = FakeNotificationCenter()
        let days = DayScheduleModel(bridge: vault.bridge)
        let window = DayWindowItem(wake: "07:00:00", sleep: "23:00:00")
        for day: Weekday in [.su, .mo, .tu, .we, .th, .fr, .sa] {
            await days.set(.weekday(day: day), window: window)
        }
        _ = try await vault.bridge.reportTimeZone(tokyo)
        let (scheduler, _) = makeScheduler(vault, name, center: center)
        await scheduler.start()
        scheduler.stopObservingTimeChanges()

        let identifier = NotificationCatalog.windDown
        let added = await center.scheduled.filter { $0.identifier == identifier }
        let first = try #require(added.first, "a sleep time schedules wind-down")
        #expect(added.count == 1)
        #expect(first.category == .catalog(.windDown))
        #expect(scheduler.catalogScheduled.contains { $0.identifier == identifier })

        _ = try await vault.bridge.reportTimeZone(vancouver)
        await scheduler.reconcile()
        let replaced = await center.scheduled.filter { $0.identifier == identifier }
        let pending = await center.pending
        #expect(replaced.count == 2, "a moved wind-down is handed over again")
        #expect(replaced.last?.revision != first.revision)
        #expect(pending.count { $0 == identifier } == 1, "it replaces the held one")

        await scheduler.setKind(.windDown, enabled: false)
        let cancelled = await center.cancelled
        let after = await center.pending
        #expect(cancelled.contains(identifier))
        #expect(!after.contains(identifier))
        #expect(!scheduler.catalogScheduled.contains { $0.identifier == identifier })
        #expect(scheduler.errorMessage == nil)
        await vault.bridge.shutdown()
    }

    /// An alert the core raises reaches the notification service, and the
    /// ledger is kept, so the next pass does not post it again.
    @Test
    func anAlertTheCoreRaisesIsPostedOnceAndTheLedgerKept() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let center = FakeNotificationCenter()
        let (scheduler, preferences) = try await plannedInTokyo(vault, name, center: center)
        let device = await vault.bridge.deviceId()
        #expect(await zoneNotices(center).isEmpty, "the first zone seen is no news")

        _ = try await vault.bridge.reportTimeZone(vancouver)
        await scheduler.reconcile()

        let notice = try #require(await zoneNotices(center).first)
        #expect(notice.identifier.hasPrefix(NotificationCatalog.prefixes.alert))
        #expect(notice.postsWhenDue)
        #expect(preferences.ledger(for: device)?.contains(vancouver) == true)

        await scheduler.reconcile()
        #expect(await zoneNotices(center).count == 1, "posted once")
        await vault.bridge.shutdown()
    }

    /// Two passes that overlap — the clock-change observer and the change
    /// feed both answering one zone change — post the notice once.
    @Test
    func overlappingPassesPostAnAlertOnce() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let center = FakeNotificationCenter()
        let (scheduler, _) = try await plannedInTokyo(vault, name, center: center)

        _ = try await vault.bridge.reportTimeZone(vancouver)
        async let first: Void = scheduler.reconcile()
        async let second: Void = scheduler.reconcile()
        _ = await (first, second)

        #expect(await zoneNotices(center).count == 1)
        await vault.bridge.shutdown()
    }

    /// An alert the service refuses is not recorded as posted: the ledger
    /// keeps the old zone, and the next pass raises the notice again.
    @Test
    func aRefusedAlertIsRaisedAgainOnTheNextPass() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let center = FakeNotificationCenter()
        let (scheduler, preferences) = try await plannedInTokyo(vault, name, center: center)
        let device = await vault.bridge.deviceId()

        await center.refuse(prefix: NotificationCatalog.prefixes.alert)
        _ = try await vault.bridge.reportTimeZone(vancouver)
        await scheduler.reconcile()
        #expect(scheduler.errorMessage != nil)
        #expect(await zoneNotices(center).isEmpty)
        #expect(preferences.ledger(for: device)?.contains(vancouver) == false)

        await center.refuse(prefix: nil)
        await scheduler.reconcile()
        #expect(await zoneNotices(center).count == 1)
        #expect(preferences.ledger(for: device)?.contains(vancouver) == true)
        #expect(scheduler.errorMessage == nil)
        await vault.bridge.shutdown()
    }

    /// A kind's switch is written where its key lives: sync stalled to this
    /// device alone, the zone notice to the vault, so it follows the user.
    @Test
    func aSwitchIsWrittenInItsKeysOwnScope() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let (scheduler, _) = makeScheduler(vault, name, center: FakeNotificationCenter())

        await scheduler.setKind(.syncStalled, enabled: false)
        await scheduler.setKind(.timezoneChanged, enabled: true)

        guard case let .preferences(items) = try await vault.bridge.query(.preferences) else {
            Issue.record("the preferences query answered something else")
            return
        }
        let stalled = try #require(items.first { $0.key == "notifications.sync_stalled.enabled" })
        let zone = try #require(items.first { $0.key == "notifications.timezone_changed.enabled" })
        #expect(stalled.source == .overlay)
        #expect(!stalled.vaultSet, "a device key never reaches the vault")
        #expect(stalled.value == .bool(value: false))
        #expect(zone.source == .vault)
        #expect(zone.vaultSet)
        #expect(scheduler.errorMessage == nil)
        await vault.bridge.shutdown()
    }
}

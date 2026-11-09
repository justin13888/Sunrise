import Foundation
import Testing
import UserNotifications

@testable import Sunrise

/// The per-device half of the notification configuration.
@MainActor
struct NotificationPreferencesTests {
    /// A scratch defaults suite, so a test cannot rewrite the developer's own
    /// quiet hours.
    private func preferences() -> (NotificationPreferences, String) {
        let name = "sunrise-tests-\(UUID().uuidString)"
        guard let defaults = UserDefaults(suiteName: name) else {
            fatalError("scratch defaults")
        }
        return (NotificationPreferences(defaults: defaults), name)
    }

    private func discard(_ name: String) {
        UserDefaults.standard.removePersistentDomain(forName: name)
    }

    /// A fresh install is usually somebody's only Mac, so it is the primary
    /// one. The alternative ships an app whose reminders silently never fire.
    @Test
    func aFreshInstallIsPrimaryAndFiresAtTheScheduledTime() {
        let (preferences, name) = preferences()
        defer { discard(name) }

        #expect(preferences.isEnabled)
        #expect(preferences.settings.isPrimaryDevice)
        #expect(preferences.settings.defaultLeadS == 0, "the spec's default lead time is 0")
        #expect(preferences.settings.quietHours == nil, "no window means never quiet")
    }

    /// The seam carries civil times as ISO-8601 text, and a malformed one
    /// comes back from the core as `BadTime` at query time rather than at edit
    /// time. Building it from a minute count is what makes that unreachable.
    @Test
    func aQuietWindowIsBuiltAsTextTheCoreCanParse() {
        let (preferences, name) = preferences()
        defer { discard(name) }

        preferences.quietHoursEnabled = true
        preferences.quietStartMinutes = 22 * 60 + 30
        preferences.quietEndMinutes = 7 * 60

        let window = preferences.settings.quietHours
        #expect(window?.start == "22:30:00")
        #expect(window?.end == "07:00:00")
        #expect(window?.policy == .queue, "the spec's default policy is queue")
    }

    @Test
    func theDropPolicyIsCarriedThrough() {
        let (preferences, name) = preferences()
        defer { discard(name) }

        preferences.quietHoursEnabled = true
        preferences.quietPolicyIsDrop = true
        #expect(preferences.settings.quietHours?.policy == .drop)
    }

    /// The domain says a window whose start equals its end silences nothing.
    /// Sending it as "no window" beats sending one the core has to ignore.
    @Test
    func aZeroWidthWindowIsSentAsNoWindowAtAll() {
        let (preferences, name) = preferences()
        defer { discard(name) }

        preferences.quietHoursEnabled = true
        preferences.quietStartMinutes = 60
        preferences.quietEndMinutes = 60
        #expect(preferences.settings.quietHours == nil)
    }

    @Test
    func theLeadTimeReachesTheSeamInSeconds() {
        let (preferences, name) = preferences()
        defer { discard(name) }

        preferences.leadMinutes = 15
        #expect(preferences.settings.defaultLeadS == 900)
    }

    /// Every value the picker offers has to survive the trip.
    @Test
    func everyClockChoiceFormatsAsAValidCivilTime() {
        for minutes in NotificationPreferences.clockChoices {
            let text = NotificationPreferences.civilTime(minutesPastMidnight: minutes)
            #expect(text.count == 8)
            #expect(text.hasSuffix(":00"))
        }
        #expect(NotificationPreferences.civilTime(minutesPastMidnight: 0) == "00:00:00")
        #expect(NotificationPreferences.civilTime(minutesPastMidnight: 1439) == "23:59:00")
    }

    /// Settings live in the core's preferences, on this device's overlay, so
    /// they survive a restart of the app: a second object bound to the same
    /// vault reads back what the first wrote.
    @Test
    func settingsPersistAcrossARestartThroughTheCore() async throws {
        let vault = try await TestVault()
        let (first, name) = preferences()
        defer { discard(name) }

        await first.bind(to: vault.bridge)
        #expect(first.isPrimaryDevice)
        let claimed = try await preferenceItems(in: vault.bridge)["notifications.primary_device"]
        #expect(claimed != nil, "an unset register is claimed")
        first.isEnabled = false
        first.leadMinutes = 30
        first.quietHoursEnabled = true
        first.quietPolicyIsDrop = true
        await first.saved()
        #expect(first.errorMessage == nil)

        let (second, other) = preferences()
        defer { discard(other) }
        await second.bind(to: vault.bridge)
        #expect(!second.isEnabled)
        #expect(second.leadMinutes == 30)
        #expect(second.quietHoursEnabled)
        #expect(second.quietStartMinutes == 22 * 60)
        #expect(second.quietEndMinutes == 7 * 60)
        #expect(second.quietPolicyIsDrop)
        #expect(second.isPrimaryDevice)

        let items = try await preferenceItems(in: vault.bridge)
        #expect(items["notifications.task_reminder.lead_s"] == .uint(value: 1800))
        #expect(items["notifications.enabled"] == .bool(value: false))

        second.quietHoursEnabled = false
        await second.saved()
        let (third, last) = preferences()
        defer { discard(last) }
        await third.bind(to: vault.bridge)
        #expect(!third.quietHoursEnabled, "turning quiet hours off clears this device's window")
        await vault.bridge.shutdown()
    }

    /// Every key an earlier build kept in `UserDefaults` moves into the core
    /// once, and is deleted: a second bind finds nothing left to move, and
    /// reads the values back from the core alone.
    @Test
    func eachOldKeyMigratesOnceAndIsDeleted() async throws {
        let vault = try await TestVault()
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        let defaults = try #require(UserDefaults(suiteName: name))
        let old: [String: Any] = [
            "notify.enabled": false,
            "notify.isPrimaryDevice": true,
            "notify.leadMinutes": 15,
            "notify.quietHours.enabled": true,
            "notify.quietHours.startMinutes": 23 * 60,
            "notify.quietHours.endMinutes": 6 * 60 + 30,
            "notify.quietHours.drop": true
        ]
        for (key, value) in old { defaults.set(value, forKey: key) }
        defaults.set("{}", forKey: "notify.ledger.dev")

        let first = NotificationPreferences(defaults: defaults)
        #expect(Set(first.legacyValues().map(\.key)) == Set(old.keys))
        await first.bind(to: vault.bridge)
        #expect(first.errorMessage == nil)
        for key in old.keys {
            #expect(defaults.object(forKey: key) == nil, "\(key) is deleted once moved")
        }
        #expect(first.ledger(for: "dev") == "{}", "the ledger is device state and stays")
        #expect(!first.isEnabled)
        #expect(first.leadMinutes == 15)
        #expect(first.quietHoursEnabled)
        #expect(first.quietStartMinutes == 23 * 60)
        #expect(first.quietEndMinutes == 6 * 60 + 30)
        #expect(first.quietPolicyIsDrop)

        let second = NotificationPreferences(defaults: defaults)
        #expect(second.legacyValues().isEmpty, "nothing left to move")
        await second.bind(to: vault.bridge)
        #expect(!second.isEnabled)
        #expect(second.leadMinutes == 15)
        #expect(second.quietPolicyIsDrop)
        await vault.bridge.shutdown()
    }

    /// The core's preferences, by key, as `Query::Preferences` reads them.
    private func preferenceItems(in bridge: CoreBridge) async throws -> [String: PreferenceValue] {
        guard case let .preferences(items) = try await bridge.query(.preferences) else { return [:] }
        var values: [String: PreferenceValue] = [:]
        for item in items {
            values[item.key] = item.value
        }
        return values
    }

    /// The one value a screen watches. Two reads of an unchanged object have
    /// to compare equal, or every read would trigger a reschedule.
    @Test
    func thePolicyIsStableUntilSomethingActuallyChanges() {
        let (preferences, name) = preferences()
        defer { discard(name) }

        #expect(preferences.policy == preferences.policy)
        let before = preferences.policy
        preferences.isPrimaryDevice = false
        #expect(preferences.policy != before)
    }
}

/// Authorization, and the promise that refusing it costs nothing else.
struct NotificationAuthorizationTests {
    /// `docs/08-features/notifications.md` never says the app degrades, and it
    /// does not have to: reminders are one surface of a local-first vault.
    /// Every state has to say so, or a user who declined is left believing
    /// something is broken.
    @Test
    func everyRefusedStateStillSaysTheAppWorks() {
        let refused: [NotificationAuthorization] = [
            .notDetermined, .denied, .unavailable("service is down")
        ]
        for status in refused {
            #expect(!status.allowsScheduling)
            #expect(!status.explanation.isEmpty)
            #expect(!status.summary.isEmpty)
        }
        #expect(NotificationAuthorization.denied.explanation.contains("Nothing else is affected"))
    }

    /// Provisional is Apple's try-before-you-allow path. Refusing to schedule
    /// under it would turn that into a broken feature.
    @Test
    func provisionalDeliveryStillCountsAsPermission() {
        #expect(NotificationAuthorization.provisional.allowsScheduling)
        #expect(NotificationAuthorization.authorized.allowsScheduling)
    }

    /// Asking twice is a no-op the system answers from its own record, so a
    /// button that appeared to do nothing would be worse than no button.
    @Test
    func onlyAnUnaskedStateOffersThePrompt() {
        #expect(NotificationAuthorization.notDetermined.canRequest)
        for status in [NotificationAuthorization.denied, .authorized, .provisional] {
            #expect(!status.canRequest)
        }
    }

    /// A status this build has never heard of must not be read as permission.
    @Test
    func theSystemsOwnStatusesLiftCorrectly() {
        #expect(NotificationAuthorization(.notDetermined) == .notDetermined)
        #expect(NotificationAuthorization(.denied) == .denied)
        #expect(NotificationAuthorization(.authorized) == .authorized)
        #expect(NotificationAuthorization(.provisional) == .provisional)
    }
}

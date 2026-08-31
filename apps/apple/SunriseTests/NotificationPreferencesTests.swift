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

    /// Settings are per device and never synced, so they must survive a
    /// restart of the app on their own.
    @Test
    func settingsPersistAcrossARestart() {
        let name = "sunrise-tests-\(UUID().uuidString)"
        defer { discard(name) }
        guard let defaults = UserDefaults(suiteName: name) else {
            Issue.record("scratch defaults")
            return
        }

        let first = NotificationPreferences(defaults: defaults)
        first.isPrimaryDevice = false
        first.leadMinutes = 30
        first.quietHoursEnabled = true

        let second = NotificationPreferences(defaults: defaults)
        #expect(!second.isPrimaryDevice)
        #expect(second.leadMinutes == 30)
        #expect(second.quietHoursEnabled)
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

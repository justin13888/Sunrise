import Foundation
import Testing

@testable import Sunrise

/// `docs/08-features/keyboard.md` §Discoverability, and the settings half of
/// `docs/07-clients/parity-matrix.md` §Vim-mode opt-in.
@MainActor
struct KeyboardTipsTests {
    private func scratch() throws -> (UserDefaults, String) {
        let suite = "sunrise-tests-\(UUID().uuidString)"
        return (try #require(UserDefaults(suiteName: suite)), suite)
    }

    /// "Opt-in" is the load-bearing word in the spec. A coachmark that arrived
    /// switched on would put a banner in front of every user who already knows
    /// the keys, and would make the toggle an opt-*out*.
    @Test
    func keyboardTipsAreOffUntilSomebodyAsksForThem() throws {
        let (defaults, suite) = try scratch()
        defer { defaults.removePersistentDomain(forName: suite) }

        let tips = KeyboardTips(defaults: defaults)
        #expect(!tips.isEnabled)
    }

    /// Per device and persisted: the answer given on the first-run screen is
    /// what the vault window reads afterwards, and it is not asked twice.
    @Test
    func theAnswerGivenOnTheFirstRunScreenSurvivesIntoTheWindow() throws {
        let (defaults, suite) = try scratch()
        defer { defaults.removePersistentDomain(forName: suite) }

        let coachmark = KeyboardTips(defaults: defaults)
        coachmark.isEnabled = true

        #expect(KeyboardTips(defaults: defaults).isEnabled, "the window reads what was ticked")

        // Dismissing the banner is the same switch, not a per-instance hide:
        // somebody who has read the tip has read it.
        let window = KeyboardTips(defaults: defaults)
        window.isEnabled = false
        #expect(!KeyboardTips(defaults: defaults).isEnabled)
    }

    /// The tip has to name something that works before there is any data —
    /// which is what makes it a first-run coachmark rather than a hint.
    @Test
    func theTipNamesTheTwoShortcutsThatWorkWithAnEmptyVault() {
        #expect(KeyboardTips.hint.contains("?"))
        #expect(KeyboardTips.hint.contains("⌘⇧P"))
        #expect(KeyboardTips.enabledKey == "keyboard.tips")
    }

    /// Vim mode is reachable from Settings *and* from the `?` sheet, and the
    /// spec asks for the setting. One setting with two entry points is a
    /// convenience; two copies of it is a bug — so both bind to the same
    /// object, and flipping either is flipping the one preference.
    @Test
    func theVimToggleInSettingsAndTheCheatSheetAreTheSameSetting() throws {
        let (defaults, suite) = try scratch()
        defer { defaults.removePersistentDomain(forName: suite) }

        let preferences = KeyboardPreferences(defaults: defaults)
        let settings = AccountView(
            settings: AppSettings(defaults: defaults),
            account: AccountModel(),
            notifications: NotificationPreferences(defaults: defaults),
            deviceID: "device",
            hotkey: .active,
            authorization: .authorized,
            scheduledCount: 0,
            signIn: {},
            allowNotifications: {},
            keyboard: preferences,
            session: nil
        )
        let sheet = CheatSheetView(preferences: preferences, hasList: true, dismiss: {})

        #expect(settings.keyboard === sheet.preferences)

        settings.keyboard.vimMode = true
        #expect(sheet.preferences.vimMode, "the sheet is already showing the vim section")
        #expect(
            KeyboardPreferences(defaults: defaults).vimMode,
            "and it went to `editor.vim_mode`, where the spec says it lives"
        )
    }
}

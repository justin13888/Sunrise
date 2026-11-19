import Foundation
import Testing

@testable import Sunrise

/// `docs/08-features/keyboard.md` §Discoverability, and the settings half of
/// `docs/07-clients/parity-matrix.md` §Vim-mode opt-in.
@MainActor
struct KeyboardTipsTests {
    /// A scratch defaults suite and a scratch directory, under one name, so
    /// no case writes the developer's own settings.
    private func scratch() throws -> (UserDefaults, URL) {
        let name = "sunrise-tests-\(UUID().uuidString)"
        let defaults = try #require(UserDefaults(suiteName: name))
        return (defaults, FileManager.default.temporaryDirectory.appending(path: name))
    }

    private func discard(_ directory: URL) {
        UserDefaults.standard.removePersistentDomain(forName: directory.lastPathComponent)
        try? FileManager.default.removeItem(at: directory)
    }

    /// "Opt-in" is the load-bearing word in the spec. A coachmark that arrived
    /// switched on would put a banner in front of every user who already knows
    /// the keys, and would make the toggle an opt-*out*.
    @Test
    func keyboardTipsAreOffUntilSomebodyAsksForThem() throws {
        let (defaults, directory) = try scratch()
        defer { discard(directory) }

        let tips = KeyboardTips(defaults: defaults, directory: directory)
        #expect(!tips.isEnabled)
    }

    /// Per device and persisted: the answer given on the first-run screen is
    /// what the vault window reads afterwards, and it is not asked twice. It
    /// is the core's client setting, not a `UserDefaults` key.
    @Test
    func theAnswerGivenOnTheFirstRunScreenSurvivesIntoTheWindow() throws {
        let (defaults, directory) = try scratch()
        defer { discard(directory) }

        let coachmark = KeyboardTips(defaults: defaults, directory: directory)
        coachmark.isEnabled = true

        let path = directory.path(percentEncoded: false)
        #expect(try clientSettings(dir: path).keyboardTips == true, "kept in the client settings file")
        #expect(defaults.object(forKey: KeyboardTips.legacyKey) == nil, "and not in `UserDefaults`")
        #expect(
            KeyboardTips(defaults: defaults, directory: directory).isEnabled,
            "the window reads what was ticked"
        )

        // Dismissing the banner is the same switch, not a per-instance hide:
        // somebody who has read the tip has read it.
        let window = KeyboardTips(defaults: defaults, directory: directory)
        window.isEnabled = false
        #expect(!KeyboardTips(defaults: defaults, directory: directory).isEnabled)
    }

    /// The value an earlier build kept in `UserDefaults` moves into the client
    /// settings file once, and its key is deleted; the next build reads the
    /// file alone.
    @Test
    func theOldKeyMigratesOnceAndIsDeleted() throws {
        let (defaults, directory) = try scratch()
        defer { discard(directory) }
        defaults.set(true, forKey: "keyboard.tips")

        let first = KeyboardTips(defaults: defaults, directory: directory)
        #expect(first.isEnabled)
        #expect(defaults.object(forKey: "keyboard.tips") == nil, "deleted once moved")
        let path = directory.path(percentEncoded: false)
        #expect(try clientSettings(dir: path).keyboardTips == true)

        let second = KeyboardTips(defaults: defaults, directory: directory)
        #expect(second.isEnabled, "read from the file, with the old key gone")
    }

    /// An old `false` is the default, so it moves as "unset" and still
    /// deletes its key.
    @Test
    func anOldOffMigratesAsUnsetAndIsDeleted() throws {
        let (defaults, directory) = try scratch()
        defer { discard(directory) }
        defaults.set(false, forKey: "keyboard.tips")

        #expect(!KeyboardTips(defaults: defaults, directory: directory).isEnabled)
        #expect(defaults.object(forKey: "keyboard.tips") == nil)
        #expect(try clientSettings(dir: directory.path(percentEncoded: false)).keyboardTips == nil)
    }

    /// The tip has to name something that works before there is any data —
    /// which is what makes it a first-run coachmark rather than a hint.
    @Test
    func theTipNamesTheTwoShortcutsThatWorkWithAnEmptyVault() {
        #expect(KeyboardTips.hint.contains("?"))
        #expect(KeyboardTips.hint.contains("⇧⌘P"))
        #expect(KeyboardTips.legacyKey == "keyboard.tips")
    }

    /// The coachmark and the tip print their chords from the keymap, in the
    /// keymap's order — ⇧⌘, the order a Mac menu prints — rather than a
    /// hand-typed ⌘⇧ that disagrees with every menu and the cheat sheet.
    @Test
    func theTipsPrintTheKeymapsOwnChords() {
        #expect(KeyboardTips.hint.contains(Keymap.shortcutLabel(for: .commandPalette)))
        #expect(KeyboardTipsCoachmark.text.contains(Keymap.shortcutLabel(for: .quickCaptureGlobal)))
        #expect(!KeyboardTips.hint.contains("⌘⇧"))
        #expect(!KeyboardTipsCoachmark.text.contains("⌘⇧"))
    }

    /// Vim mode is reachable from Settings *and* from the `?` sheet, and the
    /// spec asks for the setting. One setting with two entry points is a
    /// convenience; two copies of it is a bug — so both bind to the same
    /// object, and flipping either is flipping the one preference.
    @Test
    func theVimToggleInSettingsAndTheCheatSheetAreTheSameSetting() throws {
        let (defaults, directory) = try scratch()
        defer { discard(directory) }

        let preferences = KeyboardPreferences(defaults: defaults, directory: directory)
        let settings = AccountViewFixture.make(
            settings: AppSettings(defaults: defaults, directory: directory),
            account: AccountModel(),
            notifications: NotificationPreferences(defaults: defaults, directory: directory),
            deviceID: "device",
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
    }
}

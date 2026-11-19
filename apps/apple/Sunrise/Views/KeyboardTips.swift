import SwiftUI

/// Whether this Mac shows keyboard tips.
///
/// `docs/08-features/keyboard.md` §Discoverability asks for three things, and
/// two of them already existed: `?` opens a cheat sheet in any view, and the
/// command palette prints the binding beside every command. The third — "new
/// users see an opt-in *show keyboard tips* coachmark" — is this.
///
/// **Opt-in means off.** The coachmark on the first-run screen is what new
/// users see; the tips themselves only start once somebody ticks the box. A
/// default of `true` would make the word "opt-in" a lie and put a banner in
/// front of every user who already knows the keys.
///
/// A client setting rather than a preference: `keyboard_tips` in the core's
/// `client-settings.json`, in ``directory`` (#1182). Not in the vault, because
/// which Mac someone drives with the keyboard is a fact about the Mac, and
/// not a vault preference at all, because the coachmark asks on the first-run
/// screen, before there is a vault to write to.
///
/// The value an earlier build kept in `UserDefaults` under ``legacyKey``
/// moves into the file the first time this is built, and the key is deleted
/// once written.
@MainActor
@Observable
final class KeyboardTips {
    /// Off is written as "unset", the file's default, rather than `false`.
    var isEnabled: Bool {
        didSet { _ = try? setKeyboardTips(dir: path, shown: isEnabled ? true : nil) }
    }

    /// The `UserDefaults` key an earlier build kept this under.
    static let legacyKey = "keyboard.tips"

    /// The one sentence a tip is. Short on purpose: a coachmark that has to be
    /// read twice has already failed. The keys are the keymap's, not typed out
    /// here, so the tip cannot teach a chord the app does not answer.
    static var hint: String {
        L10n.Keyboard.tipHint(palette: Keymap.shortcutLabel(for: .commandPalette))
    }

    /// Where the client settings file lives.
    let directory: URL

    private var path: String { directory.path(percentEncoded: false) }

    /// `defaults` is read only for the value an earlier build kept there.
    init(defaults: UserDefaults = .standard, directory: URL = AppSettings.standardDirectory) {
        self.directory = directory
        let path = directory.path(percentEncoded: false)
        let legacy = defaults.object(forKey: Self.legacyKey) as? Bool
        if let legacy, (try? setKeyboardTips(dir: path, shown: legacy ? true : nil)) != nil {
            defaults.removeObject(forKey: Self.legacyKey)
        }
        // A value the file refused stays in `UserDefaults`, and is still this
        // device's setting until a later build moves it.
        isEnabled = (try? clientSettings(dir: path))?.keyboardTips ?? legacy ?? false
    }
}

/// The first-run coachmark: what the keyboard does here, and the offer.
///
/// On the onboarding screen rather than over the vault, because that is the one
/// moment the app has someone's attention and nothing of theirs to lose. The
/// two shortcuts named are the two that work before anything exists: `?` needs
/// no data, and ⇧⌘N is the reason the app is worth leaving running.
struct KeyboardTipsCoachmark: View {
    @Bindable var tips: KeyboardTips

    /// Built from the keymap, like ``KeyboardTips/hint``.
    static var text: String {
        L10n.Keyboard.coachmark(shortcut: Keymap.shortcutLabel(for: .quickCaptureGlobal))
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Label(L10n.Keyboard.keyboardFirst, systemImage: "keyboard")
                .font(.callout.weight(.medium))
            Text(Self.text)
            .font(.caption)
            .foregroundStyle(.secondary)
            Toggle(L10n.Keyboard.showTips, isOn: $tips.isEnabled)
                .accessibilityIdentifier("onboarding.keyboardTips")
        }
        .padding(12)
        .frame(maxWidth: 380, alignment: .leading)
        .background(.quaternary.opacity(0.5), in: .rect(cornerRadius: 10))
    }
}

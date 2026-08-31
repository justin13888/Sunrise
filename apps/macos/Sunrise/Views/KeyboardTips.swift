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
/// `UserDefaults` rather than the vault, like every other keyboard preference:
/// which Mac someone drives with the keyboard is a fact about the Mac.
@MainActor
@Observable
final class KeyboardTips {
    var isEnabled: Bool {
        didSet { defaults.set(isEnabled, forKey: KeyboardTips.enabledKey) }
    }

    static let enabledKey = "keyboard.tips"

    /// The one sentence a tip is. Short on purpose: a coachmark that has to be
    /// read twice has already failed.
    static let hint = "Press ? for the keyboard shortcuts, or ⌘⇧P for the command palette."

    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        isEnabled = defaults.bool(forKey: KeyboardTips.enabledKey)
    }
}

/// The first-run coachmark: what the keyboard does here, and the offer.
///
/// On the onboarding screen rather than over the vault, because that is the one
/// moment the app has someone's attention and nothing of theirs to lose. The
/// two shortcuts named are the two that work before anything exists: `?` needs
/// no data, and ⌘⇧N is the reason the app is worth leaving running.
struct KeyboardTipsCoachmark: View {
    @Bindable var tips: KeyboardTips

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Label("Sunrise is keyboard-first", systemImage: "keyboard")
                .font(.callout.weight(.medium))
            Text(
                """
                Press ? in any view for the shortcuts, and ⌘⇧N to capture a \
                thought from whatever app you are in.
                """
            )
            .font(.caption)
            .foregroundStyle(.secondary)
            Toggle("Show keyboard tips as I go", isOn: $tips.isEnabled)
                .accessibilityIdentifier("onboarding.keyboardTips")
        }
        .padding(12)
        .frame(maxWidth: 380, alignment: .leading)
        .background(.quaternary.opacity(0.5), in: .rect(cornerRadius: 10))
    }
}

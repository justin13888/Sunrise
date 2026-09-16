import Sparkle
import SwiftUI

/// The names shared with the appcast, in one place because they are a contract
/// with a file generated somewhere else entirely.
///
/// `beta` is matched by string against `<sparkle:channel>` in the feed
/// `.github/scripts/appcast.py` writes. Renaming it here alone would not break
/// a build — it would silently empty the beta channel, which is the failure
/// mode a shared constant with a comment is cheap insurance against.
enum SoftwareUpdateChannel {
    /// The channel a prerelease lands on. A stable release carries no channel
    /// element at all, which is Sparkle's spelling of "everyone gets this".
    static let beta = "beta"

    /// `UserDefaults` key behind the Include Beta Updates menu item.
    ///
    /// Read by ``UpdaterChannelDelegate`` rather than passed to it, because
    /// Sparkle asks for the allowed channels at the moment it checks — so the
    /// toggle takes effect on the next check with nothing to wire up.
    static let includeBetaKey = "dev.sunrise.updates.includeBeta"
}

/// Sparkle's answer to "which channels is this installation subscribed to?".
///
/// Deliberately stateless: it reads `UserDefaults` on every call instead of
/// caching, so the one place the preference lives is the same place
/// `@AppStorage` writes it.
private final class UpdaterChannelDelegate: NSObject, SPUUpdaterDelegate {
    func allowedChannels(for updater: SPUUpdater) -> Set<String> {
        UserDefaults.standard.bool(forKey: SoftwareUpdateChannel.includeBetaKey)
            ? [SoftwareUpdateChannel.beta]
            : []
    }
}

/// The macOS updater, and the check that decides whether there is one.
///
/// ADR-0038 records the decision this file implements, including the part that
/// is not a packaging detail: Sparkle authorises a *change* of appcast key with
/// the app's Apple code signature, so the EdDSA key is subordinate to the
/// Developer ID certificate in lifecycle rather than a second, co-equal trust
/// root. Read that ADR before assuming it is the weaker of the two — Sparkle
/// accepts an update on *either* credential, so the appcast key ships code.
///
/// macOS only. `apps/apple/macOS/` is compiled into the Mac target alone, which
/// is why nothing here needs an `#if os(macOS)` and why the iOS target does not
/// link Sparkle at all — ADR-0039 records that iOS has no direct-download
/// channel to update over.
@MainActor
enum SoftwareUpdate {
    /// The running updater, or `nil` when this build cannot verify an update.
    ///
    /// `nil` is the fail-closed case and it is reachable today: `SUPublicEDKey`
    /// is empty in `apps/apple/project.yml` until the repository owner
    /// generates the key pair. Starting Sparkle with no public key would give
    /// the app an update path it cannot authenticate, which is strictly worse
    /// than having none — so the controller is not created, no check is ever
    /// scheduled, and ``SoftwareUpdateMenuItems`` says so where a user can read
    /// it.
    static let controller: SPUStandardUpdaterController? = {
        guard hasPublicKey else { return nil }
        return SPUStandardUpdaterController(
            startingUpdater: true,
            updaterDelegate: channelDelegate,
            userDriverDelegate: nil
        )
    }()

    /// Why the Check for Updates item is unavailable, or `nil` when it is not.
    ///
    /// A sentence rather than a `Bool`, matching `CommandMenuItem`: a dimmed
    /// item teaches a sighted user "not now" and a VoiceOver user nothing at
    /// all, and `docs/10-cross-cutting/accessibility.md` forbids a state
    /// carried by appearance alone.
    static var unavailable: String? {
        hasPublicKey
            ? nil
            : "This build carries no update-signing public key, so it cannot "
                + "verify an update. Download a release from the project's "
                + "Releases page instead."
    }

    /// Retained here because Sparkle holds its delegate weakly; a local would
    /// be deallocated before the first check and the beta channel would
    /// silently stop being offered.
    private static let channelDelegate = UpdaterChannelDelegate()

    private static var hasPublicKey: Bool {
        let key = Bundle.main.object(forInfoDictionaryKey: "SUPublicEDKey") as? String
        return !(key ?? "").isEmpty
    }
}

/// The app menu's update items: check now, and whether to be offered betas.
///
/// In the menu bar rather than in settings because the Mac's own convention
/// puts Check for Updates directly under About, and because the settings form
/// is `Sunrise/Views/AccountView.swift` — shared with iOS, where none of this
/// exists. Keeping the surface here is what keeps the shared tree shared
/// (ADR-0028 revisit trigger 4).
struct SoftwareUpdateMenuItems: View {
    @AppStorage(SoftwareUpdateChannel.includeBetaKey) private var includeBeta = false

    var body: some View {
        Button("Check for Updates…") {
            SoftwareUpdate.controller?.updater.checkForUpdates()
        }
        .disabled(SoftwareUpdate.unavailable != nil)
        .help(SoftwareUpdate.unavailable ?? "")
        .accessibilityHint(SoftwareUpdate.unavailable ?? "")

        Toggle("Include Beta Updates", isOn: $includeBeta)
            .disabled(SoftwareUpdate.unavailable != nil)
            .help(SoftwareUpdate.unavailable
                ?? "Offer release candidates and betas as well as stable releases.")
    }
}

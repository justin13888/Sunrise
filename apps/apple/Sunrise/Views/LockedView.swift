import SwiftUI

/// A vault that exists and cannot be opened.
///
/// Deliberately offers no way to create a new one. The obvious repair is the
/// destructive one, and the screen a user reaches in a panic is the worst
/// place to put it.
///
/// It does offer the repair it has been *naming* since it was written.
/// `LockReason.keyMissingForExistingVault` tells the user to "pair with a
/// device that still has it", and until this button existed the app had no way
/// to do that — which made the one instruction on the screen a dead end. That
/// is worse than saying nothing: it reads as though the user is missing
/// something.
struct LockedView: View {
    let reason: SessionModel.LockReason
    let retry: () async -> Void
    /// The session this screen is a phase of. Passed in by ``RootView``, which
    /// owns it; optional only so a preview can stand this screen up without
    /// one.
    let session: SessionModel?

    @State private var pairing: PairingModel?
    @State private var settings = AppSettings()

    var body: some View {
        VStack(spacing: 16) {
            Image(systemName: "lock.trianglebadge.exclamationmark")
                .font(.system(size: 44))
                .foregroundStyle(.orange)
            Text("Sunrise is locked")
                .font(.title.weight(.semibold))
            Text(reason.summary)
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 460)

            HStack(spacing: 12) {
                // "Unlock" when the user closed it themselves, "Try again"
                // when the vault refused — the same button, but a retry and a
                // deliberate reopen are not the same request.
                Button(reason.repairTitle) { Task { await retry() } }
                    .buttonStyle(.borderedProminent)
                    .accessibilityIdentifier("locked.retry")
                #if os(macOS)
                // macOS only, and not for want of an iOS equivalent: iOS has
                // no Keychain Access app and no user-facing keychain at all.
                // An unavailable keychain there is repaired by unlocking the
                // device or reinstalling, neither of which an app can offer a
                // button for — so iOS shows the explanation and the retry
                // above, and stops there rather than pointing at nothing.
                if case .keychainUnavailable = reason {
                    Button("Open Keychain Access") {
                        Platform.openExternal(
                            URL(filePath: "/System/Applications/Utilities/Keychain Access.app")
                        )
                    }
                }
                #endif
                if reason == .keyMissingForExistingVault {
                    Button("Pair with a device…") { pairing = makePairing() }
                        .accessibilityIdentifier("locked.pair")
                }
            }
            .controlSize(.large)

            if reason == .keyMissingForExistingVault {
                Text(
                    """
                    Pairing brings the key back from a device that still has it. \
                    The data already on this \(Platform.deviceName) stays where it is — the key is \
                    the only thing that was missing.
                    """
                )
                .font(.caption)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 460)
            }
        }
        .padding(40)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .sheet(item: $pairing) { model in
            PairingView(model: model) { pairing = nil }
        }
    }

    private func makePairing() -> PairingModel {
        PairingModel(
            intent: .addThisMac,
            relayURL: settings.relayURL.trimmed,
            adopt: { [session] root, bundle in
                await session?.adoptPairing(root: root, bundle: bundle)
            }
        )
    }
}

#Preview {
    LockedView(reason: .keyMissingForExistingVault, retry: {}, session: nil)
}

import AppKit
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
    /// Defaulted so `RootView` — which does not pass a session down — keeps
    /// compiling, and injectable so a test can drive this without the
    /// process-wide one.
    var session: SessionModel? = SessionModel.active

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
                Button("Try again") { Task { await retry() } }
                    .buttonStyle(.borderedProminent)
                if case .keychainUnavailable = reason {
                    Button("Open Keychain Access") {
                        NSWorkspace.shared.open(
                            URL(filePath: "/System/Applications/Utilities/Keychain Access.app")
                        )
                    }
                }
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
                    The data already on this Mac stays where it is — the key is \
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
            adopt: { [session] root in await session?.adoptVaultRoot(root) }
        )
    }
}

#Preview {
    LockedView(reason: .keyMissingForExistingVault, retry: {}, session: nil)
}

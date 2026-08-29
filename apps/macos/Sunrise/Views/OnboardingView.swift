import SwiftUI

/// First run: there is no vault and no key, so nothing can be lost by making
/// one.
///
/// Two branches, not one. "Create my vault" is right for a first Mac and wrong
/// for a second: someone who already has a vault and takes it would end up
/// with two unrelated encrypted stores and no way to merge them. Pairing is
/// how the existing vault reaches this machine, and it has to be offered here,
/// where the choice is actually being made.
struct OnboardingView: View {
    let create: () async -> Void
    /// Defaulted so `RootView` — which does not pass a session down — keeps
    /// compiling, and injectable so a test can drive this without the
    /// process-wide one.
    var session: SessionModel? = SessionModel.active

    @State private var isWorking = false
    @State private var pairing: PairingModel?
    @State private var settings = AppSettings()

    var body: some View {
        VStack(spacing: 16) {
            Image(systemName: "sunrise")
                .font(.system(size: 48))
                .foregroundStyle(.orange)
            Text("Welcome to Sunrise")
                .font(.largeTitle.weight(.semibold))
            Text(
                """
                Your tasks are encrypted on this Mac with a key only you hold. \
                Sunrise stores it in your Keychain — it never leaves the device, \
                and no server can read your data with or without it.
                """
            )
            .font(.callout)
            .foregroundStyle(.secondary)
            .multilineTextAlignment(.center)
            .frame(maxWidth: 420)

            Button("Create my vault") {
                isWorking = true
                Task {
                    await create()
                    isWorking = false
                }
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.large)
            .disabled(isWorking)
            .accessibilityIdentifier("onboarding.create")

            if isWorking {
                ProgressView().controlSize(.small)
            }

            Divider().frame(maxWidth: 320)

            VStack(spacing: 6) {
                Text("Already using Sunrise on another Mac?")
                    .font(.callout)
                Button("Pair with that device") { pairing = makePairing() }
                    .controlSize(.large)
                    .disabled(isWorking)
                    .accessibilityIdentifier("onboarding.pair")
                Text(
                    """
                    Adopts the vault you already have, instead of starting a \
                    second one.
                    """
                )
                .font(.caption)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 380)
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

/// So a `PairingModel` can drive a `sheet(item:)`.
///
/// Identity is the object's own — one live pairing per sheet — rather than
/// anything derived from its state, which changes on every leg and would
/// rebuild the sheet underneath the user.
extension PairingModel: Identifiable {
    nonisolated var id: ObjectIdentifier { ObjectIdentifier(self) }
}

#Preview {
    OnboardingView(create: {}, session: nil)
}
